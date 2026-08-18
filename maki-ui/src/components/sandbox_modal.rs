use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use maki_agent::AgentConfig;
use maki_sandbox::Sandbox;
use maki_sandbox::namespace::{EnvEntry, NamespaceConfig};
use maki_sandbox::profiles::{self, MountUsage, SandboxProfile};

use crate::components::ModalScroll;
use crate::components::Overlay;
use crate::components::modal::{CHROME_LINES, Modal};
use crate::components::scrollbar::render_vertical_scrollbar;
use crate::theme;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

const TITLE: &str = " Sandbox ";
const WIDTH_PERCENT: u16 = 65;
const MAX_HEIGHT_PERCENT: u16 = 85;

const BROWSE_SETUP_MSG: maki_sandbox::ipc::SetupMessage = maki_sandbox::ipc::SetupMessage::browse();

#[derive(Clone, Debug, PartialEq)]
enum Mode {
    Info,
    Browse,
    Shell,
}

/// Snapshot of sandbox configuration for display.
pub struct SandboxInfo {
    pub enabled: bool,
    pub env_entries: Vec<EnvEntry>,
    pub workspace_dir: String,
    pub workspace_name: String,
    pub home_mounts: Vec<(String, String)>,
    /// (profile, enabled) pairs shown on the info tab.
    pub profiles: Vec<(SandboxProfile, bool)>,
    /// Extra host directories to bind-mount into the workspace.
    pub extra_workspace_dirs: Vec<(String, String)>,
}

struct FileEntry {
    name: String,
    is_dir: bool,
}

/// Sandbox filesystem browser that talks to a sandbox child process via IPC.
///
/// On creation it spawns a browse-only sandbox child (empty code = no interpreter).
/// Navigation (enter, go_up) sends Ls queries over the Unix socket.
/// On drop it sends Exit and waits for the child.
struct SandboxFileBrowser {
    sandbox: Arc<Sandbox>,
    cwd: String,
    entries: Vec<FileEntry>,
    cursor: usize,
    scroll: usize,
    viewport_entries: usize,
    error: Option<String>,
}

impl SandboxFileBrowser {
    fn new(sandbox: Arc<Sandbox>) -> Result<Self, String> {
        sandbox
            .setup(&BROWSE_SETUP_MSG)
            .map_err(|e| e.to_string())?;
        let pwd = sandbox.pwd().map_err(|e| e.to_string())?;
        let mut browser = Self {
            cwd: pwd,
            sandbox,
            entries: Vec::new(),
            cursor: 0,
            scroll: 0,
            viewport_entries: 0,
            error: None,
        };
        browser.refresh();
        Ok(browser)
    }

    fn refresh(&mut self) {
        let result = self.sandbox.ls(&self.cwd);
        self.entries.clear();
        match result {
            Ok(entries) => {
                self.error = None;
                for e in entries {
                    self.entries.push(FileEntry {
                        name: e.name,
                        is_dir: e.is_dir,
                    });
                }
            }
            Err(e) => {
                self.error = Some(e.to_string());
            }
        }
        self.entries.sort_by(|a, b| match (a.is_dir, b.is_dir) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.name.cmp(&b.name),
        });
        if self.cwd != "/" {
            self.entries.insert(
                0,
                FileEntry {
                    name: "..".into(),
                    is_dir: true,
                },
            );
        }
        let max = self.entries.len().saturating_sub(1);
        self.cursor = self.cursor.min(max);
    }

    fn clamp_cursor(&mut self) {
        let max = self.entries.len().saturating_sub(1);
        self.cursor = self.cursor.min(max);
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        } else if self.entries.len() > self.viewport_entries
            && self.cursor >= self.scroll + self.viewport_entries
        {
            self.scroll = self
                .cursor
                .saturating_add(1)
                .saturating_sub(self.viewport_entries);
        }
    }

    fn visible_entries(&self) -> &[FileEntry] {
        let start = self.scroll.min(self.entries.len());
        let end = (self.scroll + self.viewport_entries).min(self.entries.len());
        &self.entries[start..end]
    }

    fn scroll_offset(&self) -> usize {
        self.scroll
    }

    fn total_entries(&self) -> usize {
        self.entries.len()
    }

    fn enter(&mut self) {
        let Some(entry) = self.entries.get(self.cursor) else {
            return;
        };
        if entry.name == ".." {
            let parent = parent_dir(&self.cwd).unwrap_or_else(|| "/".into());
            if let Err(e) = self.sandbox.cd(&parent) {
                self.error = Some(e.to_string());
                return;
            }
            self.cwd = parent;
        } else if entry.is_dir {
            let sep = if self.cwd.ends_with('/') { "" } else { "/" };
            let target = format!("{}{}{}", self.cwd, sep, entry.name);
            if let Err(e) = self.sandbox.cd(&target) {
                self.error = Some(e.to_string());
                return;
            }
            self.cwd = target;
        } else {
            return;
        }
        self.cursor = 0;
        self.scroll = 0;
        self.refresh();
    }

    fn go_up(&mut self) {
        if let Some(parent) = parent_dir(&self.cwd) {
            if let Err(e) = self.sandbox.cd(&parent) {
                self.error = Some(e.to_string());
                return;
            }
            self.cwd = parent;
            self.cursor = 0;
            self.scroll = 0;
            self.refresh();
        }
    }
}

fn parent_dir(path: &str) -> Option<String> {
    if path == "/" {
        return None;
    }
    let p = Path::new(path).parent()?;
    let result = p.to_string_lossy().to_string();
    if result.is_empty() {
        Some("/".into())
    } else {
        Some(result)
    }
}

/// A single entry in the shell output history.
struct ShellEntry {
    command: String,
    output: String,
    is_error: bool,
}

/// Sandbox interactive shell that talks to a sandbox child via IPC.
///
/// On creation it spawns a browse-only sandbox child (empty code = no interpreter).
/// Each command is sent as an Exec IPC message and the output is collected.
/// On drop it sends Exit and waits for the child.
struct SandboxShellState {
    sandbox: Arc<Sandbox>,
    cwd: String,
    input: String,
    entries: Vec<ShellEntry>,
    history: Vec<String>,
    history_pos: Option<usize>,
    error: Option<String>,
}

impl SandboxShellState {
    fn new(sandbox: Arc<Sandbox>) -> Result<Self, String> {
        sandbox
            .setup(&BROWSE_SETUP_MSG)
            .map_err(|e| e.to_string())?;
        let init_cwd = sandbox.pwd().unwrap_or_default();
        Ok(Self {
            sandbox,
            cwd: init_cwd,
            input: String::new(),
            entries: Vec::new(),
            history: Vec::new(),
            history_pos: None,
            error: None,
        })
    }

    fn exec(&mut self, command: &str) {
        match self.sandbox.exec(command) {
            Ok((output, is_error)) => {
                self.error = None;
                self.entries.push(ShellEntry {
                    command: command.to_string(),
                    output,
                    is_error,
                });
            }
            Err(e) => {
                self.error = Some(e.to_string());
            }
        }
    }

    fn submit(&mut self) {
        let command = std::mem::take(&mut self.input);
        if command.is_empty() {
            return;
        }
        self.history.push(command.clone());
        self.history_pos = None;
        self.exec(&command);
    }

    fn history_up(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let pos = self.history_pos.unwrap_or(self.history.len());
        if pos > 0 {
            let new_pos = pos - 1;
            self.history_pos = Some(new_pos);
            self.input = self.history[new_pos].clone();
        }
    }

    fn history_down(&mut self) {
        match self.history_pos {
            Some(pos) if pos + 1 < self.history.len() => {
                let new_pos = pos + 1;
                self.history_pos = Some(new_pos);
                self.input = self.history[new_pos].clone();
            }
            _ => {
                self.history_pos = None;
                self.input.clear();
            }
        }
    }
}

pub struct SandboxModal {
    open: bool,
    mode: Mode,
    scroll: ModalScroll,
    h_scroll: usize,
    info: SandboxInfo,
    sandbox: Option<Arc<Sandbox>>,
    browser: Option<SandboxFileBrowser>,
    shell: Option<SandboxShellState>,
    shell_entry_count: usize,
    profile_cursor: Option<usize>,
    spawn_error: Option<String>,
    /// Set when the user toggles `enabled` via the UI.
    enabled_changed: bool,
    /// YOLO state as shown in the checkbox (owned by the app's permissions,
    /// mirrored here for display).
    yolo: bool,
    /// Set when the user toggles YOLO via the UI.
    yolo_changed: bool,
}

impl SandboxModal {
    pub fn new(info: SandboxInfo, sandbox: Option<Arc<Sandbox>>) -> Self {
        let profile_cursor = if info.profiles.is_empty() {
            None
        } else {
            Some(0)
        };
        Self {
            open: false,
            mode: Mode::Info,
            scroll: ModalScroll::new_top(),
            h_scroll: 0,
            info,
            sandbox,
            browser: None,
            shell: None,
            shell_entry_count: 0,
            profile_cursor,
            spawn_error: None,
            enabled_changed: false,
            yolo: false,
            yolo_changed: false,
        }
    }

    /// Build the modal from the agent config, deriving the namespace
    /// layout (mounts, env) shown on the info tab.
    pub fn from_config(config: &AgentConfig, sandbox: Option<Arc<Sandbox>>) -> Self {
        let workspace_dir = std::env::current_dir().ok();
        let workspace_name = workspace_dir
            .as_ref()
            .and_then(|d| d.file_name().map(|n| n.to_string_lossy().to_string()))
            .unwrap_or_default();
        let ns_config = NamespaceConfig::from_agent_config(
            config.sandbox_allowed_env.clone(),
            &config.sandbox_allowed_paths,
            &config.sandbox_extra_dirs,
            workspace_dir.clone().unwrap_or_default(),
            workspace_name.clone(),
        );
        let home_mounts: Vec<(String, String)> = ns_config
            .home_mounts
            .iter()
            .map(|(p, name)| (p.display().to_string(), name.clone()))
            .collect();
        let extra_workspace_dirs: Vec<(String, String)> = ns_config
            .extra_workspace_dirs
            .iter()
            .map(|(p, name)| (p.display().to_string(), name.clone()))
            .collect();
        let env_entries = ns_config.effective_env();
        Self::new(
            SandboxInfo {
                enabled: config.sandbox_enabled,
                env_entries,
                workspace_dir: workspace_dir
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
                workspace_name,
                home_mounts,
                profiles: profiles::builtin_profiles()
                    .into_iter()
                    .map(|p| (p, false))
                    .collect(),
                extra_workspace_dirs,
            },
            sandbox,
        )
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn scroll(&mut self, delta: i32) {
        self.scroll.scroll(delta);
    }

    pub fn toggle(&mut self) {
        self.open = !self.open;
        if self.open {
            self.mode = Mode::Info;
            self.close_browser();
            self.close_shell();
            self.spawn_error = None;
            self.enabled_changed = false;
            self.yolo_changed = false;
            self.profile_cursor = if self.info.profiles.is_empty() {
                None
            } else {
                Some(0)
            };
        } else {
            self.close_browser();
            self.close_shell();
        }
        self.scroll.reset();
    }

    /// Returns and resets the `enabled_changed` flag.
    /// The caller should check this after `handle_key` to persist the setting.
    pub fn take_enabled_changed(&mut self) -> bool {
        std::mem::take(&mut self.enabled_changed)
    }

    /// Whether the sandbox is currently enabled.
    pub fn is_enabled(&self) -> bool {
        self.info.enabled
    }

    /// Mirror the app's live YOLO state into the checkbox for display.
    pub fn set_yolo(&mut self, yolo: bool) {
        self.yolo = yolo;
    }

    /// Returns and resets the `yolo_changed` flag.
    /// The caller should check this after `handle_key` to apply to permissions.
    pub fn take_yolo_changed(&mut self) -> bool {
        std::mem::take(&mut self.yolo_changed)
    }

    fn toggle_yolo(&mut self) {
        self.yolo = !self.yolo;
        self.yolo_changed = true;
    }

    pub fn close(&mut self) {
        self.open = false;
        self.close_browser();
        self.close_shell();
        self.spawn_error = None;
        self.scroll.reset();
        self.profile_cursor = None;
    }

    fn close_browser(&mut self) {
        self.browser.take();
    }

    fn close_shell(&mut self) {
        self.shell.take();
        self.shell_entry_count = 0;
    }

    fn rebuild_env_entries(&mut self) {
        let path_dirs: Vec<String> = self
            .info
            .profiles
            .iter()
            .filter(|(_, enabled)| *enabled)
            .flat_map(|(p, _)| &p.mounts)
            .filter(|m| m.usage == MountUsage::OnlyPath)
            .map(|m| m.sandbox_internal_path())
            .collect();
        let cfg = NamespaceConfig::new(
            vec![],
            vec![],
            std::path::PathBuf::new(),
            String::new(),
            vec![],
            vec![],
            path_dirs,
            vec![],
            vec![],
        );
        self.info.env_entries = cfg.effective_env();
    }

    fn toggle_sandbox_enabled(&mut self) {
        if !self.info.enabled {
            // Validate kernel support before enabling.
            if let Err(e) = maki_sandbox::namespace::probe() {
                self.spawn_error = Some(e.to_string());
                return;
            }
        }
        self.info.enabled = !self.info.enabled;
        self.enabled_changed = true;
        self.spawn_error = None;
    }

    fn spawn_browser(&mut self) {
        if self.browser.is_some() {
            return;
        }
        self.close_shell();
        let Some(sandbox) = self.sandbox.as_ref() else {
            self.spawn_error = Some("sandbox not available".into());
            return;
        };
        let config = self.build_namespace_config();
        if let Err(e) = sandbox.reinit(config) {
            tracing::error!(error = %e, "failed to reinit sandbox for browser");
            self.spawn_error = Some(e.to_string());
            return;
        }
        match SandboxFileBrowser::new(Arc::clone(sandbox)) {
            Ok(browser) => {
                self.browser = Some(browser);
                self.spawn_error = None;
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to spawn sandbox browser");
                self.spawn_error = Some(e);
            }
        }
    }

    fn spawn_shell(&mut self) {
        if self.shell.is_some() {
            return;
        }
        self.close_browser();
        self.shell_entry_count = 0;
        let Some(sandbox) = self.sandbox.as_ref() else {
            self.spawn_error = Some("sandbox not available".into());
            return;
        };
        let config = self.build_namespace_config();
        if let Err(e) = sandbox.reinit(config) {
            tracing::error!(error = %e, "failed to reinit sandbox for shell");
            self.spawn_error = Some(e.to_string());
            return;
        }
        match SandboxShellState::new(Arc::clone(sandbox)) {
            Ok(shell) => {
                self.shell = Some(shell);
                self.spawn_error = None;
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to spawn sandbox shell");
                self.spawn_error = Some(e);
            }
        }
    }

    fn build_namespace_config(&self) -> NamespaceConfig {
        let extra_home_mounts: Vec<(PathBuf, String)> = self
            .info
            .home_mounts
            .iter()
            .map(|(host, name)| (PathBuf::from(host), name.clone()))
            .collect();
        let extra_workspace_dirs: Vec<(PathBuf, String)> = self
            .info
            .extra_workspace_dirs
            .iter()
            .map(|(host, name)| (PathBuf::from(host), name.clone()))
            .collect();
        let enabled_profiles: Vec<profiles::SandboxProfile> = self
            .info
            .profiles
            .iter()
            .filter(|(_, enabled)| *enabled)
            .map(|(p, _)| p.clone())
            .collect();
        profiles::build_namespace_config(
            &enabled_profiles,
            PathBuf::from(&self.info.workspace_dir),
            self.info.workspace_name.clone(),
            extra_home_mounts,
            extra_workspace_dirs,
        )
    }

    pub fn handle_key(&mut self, key_event: KeyEvent) -> bool {
        match key_event.code {
            KeyCode::Esc => {
                self.close();
                true
            }
            KeyCode::Tab => {
                self.mode = match self.mode {
                    Mode::Info => {
                        if self.info.enabled {
                            self.spawn_browser();
                            Mode::Browse
                        } else {
                            Mode::Info
                        }
                    }
                    Mode::Browse => {
                        self.close_browser();
                        if self.info.enabled {
                            self.spawn_shell();
                            self.scroll.scroll_to_bottom();
                            Mode::Shell
                        } else {
                            Mode::Info
                        }
                    }
                    Mode::Shell => {
                        self.close_shell();
                        Mode::Info
                    }
                };
                self.scroll.reset();
                self.h_scroll = 0;
                true
            }
            _ if self.mode == Mode::Browse => self.handle_browse_key(key_event),
            _ if self.mode == Mode::Shell => self.handle_shell_key(key_event),
            _ if self.mode == Mode::Info && self.profile_cursor.is_some() => match key_event.code {
                KeyCode::Char('s') => {
                    self.toggle_sandbox_enabled();
                    true
                }
                KeyCode::Char('y') => {
                    self.toggle_yolo();
                    true
                }
                KeyCode::Up => {
                    if let Some(cursor) = self.profile_cursor
                        && cursor > 0
                    {
                        self.profile_cursor = Some(cursor - 1);
                    }
                    true
                }
                KeyCode::Down => {
                    if let Some(cursor) = &mut self.profile_cursor {
                        let max = self.info.profiles.len().saturating_sub(1);
                        if *cursor < max {
                            *cursor += 1;
                        }
                    }
                    true
                }
                KeyCode::Enter | KeyCode::Char(' ') => {
                    if let Some(cursor) = self.profile_cursor
                        && let Some((_, enabled)) = self.info.profiles.get_mut(cursor)
                    {
                        *enabled = !*enabled;
                        self.rebuild_env_entries();
                    }
                    true
                }
                _ => {
                    self.scroll.handle_key(key_event);
                    true
                }
            },
            _ => {
                self.scroll.handle_key(key_event);
                true
            }
        }
    }

    fn handle_browse_key(&mut self, key_event: KeyEvent) -> bool {
        let Some(browser) = &mut self.browser else {
            return true;
        };
        match key_event.code {
            KeyCode::Enter | KeyCode::Right => {
                browser.enter();
                true
            }
            KeyCode::Backspace | KeyCode::Left => {
                browser.go_up();
                true
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if browser.cursor > 0 {
                    browser.cursor -= 1;
                    browser.clamp_cursor();
                }
                true
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let max = browser.entries.len().saturating_sub(1);
                if browser.cursor < max {
                    browser.cursor += 1;
                    browser.clamp_cursor();
                }
                true
            }
            _ => true,
        }
    }

    fn handle_shell_key(&mut self, key_event: KeyEvent) -> bool {
        let Some(shell) = &mut self.shell else {
            return true;
        };
        match key_event.code {
            KeyCode::Enter => {
                shell.submit();
                self.scroll.scroll_to_bottom();
                self.h_scroll = 0;
                true
            }
            KeyCode::Up => {
                shell.history_up();
                true
            }
            KeyCode::Down => {
                shell.history_down();
                true
            }
            KeyCode::Backspace => {
                shell.input.pop();
                true
            }
            KeyCode::Char(ch) => {
                shell.input.push(ch);
                true
            }
            KeyCode::Left => {
                self.h_scroll = self.h_scroll.saturating_sub(1);
                true
            }
            KeyCode::Right => {
                self.h_scroll = self.h_scroll.saturating_add(1);
                true
            }
            KeyCode::PageUp | KeyCode::PageDown | KeyCode::Home | KeyCode::End => {
                self.scroll.handle_key(key_event);
                true
            }
            _ => true,
        }
    }

    pub fn handle_paste(&mut self, text: &str) -> bool {
        if !self.open || self.mode != Mode::Shell {
            return false;
        }
        if let Some(shell) = &mut self.shell {
            for ch in text.chars() {
                if ch == '\n' || ch == '\r' {
                    shell.submit();
                    self.scroll.scroll_to_bottom();
                } else {
                    shell.input.push(ch);
                }
            }
            return true;
        }
        false
    }

    fn render_info(&mut self, lines: &mut Vec<Line>) {
        let t = theme::current();
        let info = &self.info;

        // Clamp cursor in case profiles changed
        if let Some(cursor) = self.profile_cursor
            && cursor >= info.profiles.len()
        {
            self.profile_cursor = if info.profiles.is_empty() {
                None
            } else {
                Some(0)
            };
        }

        // Status
        lines.push(Line::from(Span::styled(
            "  Status (press s to toggle)",
            t.keybind_section,
        )));
        let status_text = if info.enabled { "enabled" } else { "disabled" };
        lines.push(Line::from(format!("    {status_text}")));

        // YOLO — skip permission prompts for all tools while on. Only shown
        // when the sandbox is enabled, as it is a companion to sandboxing.
        if info.enabled {
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                "  YOLO (press y to toggle) — skip permission prompts for all tools",
                t.keybind_section,
            )));
            let yolo_toggle = if self.yolo { "x" } else { " " };
            let yolo_style = if self.yolo {
                t.item_selected
            } else {
                Style::default()
            };
            lines.push(Line::from(vec![
                Span::styled("    [", Style::default()),
                Span::styled(yolo_toggle, yolo_style),
                Span::styled("] ", Style::default()),
                Span::styled(
                    if self.yolo { "on" } else { "off" },
                    yolo_style,
                ),
            ]));
        }

        // Profiles — right after Status, with cursor navigation and description
        if !info.profiles.is_empty() {
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                "  Profiles (navigate with ↑/↓, toggle with Enter/Space)",
                t.keybind_section,
            )));
            for (i, (profile, enabled)) in info.profiles.iter().enumerate() {
                let selected = self.profile_cursor == Some(i);
                let toggle = if *enabled { "x" } else { " " };
                let prefix = if selected { "  ▸ " } else { "    " };
                lines.push(Line::from(vec![
                    Span::styled(
                        prefix,
                        if selected {
                            t.item_selected
                        } else {
                            Style::default()
                        },
                    ),
                    Span::styled("[", Style::default()),
                    Span::styled(
                        toggle,
                        if *enabled {
                            t.item_selected
                        } else {
                            Style::default()
                        },
                    ),
                    Span::styled("] ", Style::default()),
                    Span::styled(
                        profile.name.clone(),
                        if selected {
                            t.item_selected
                        } else {
                            Style::default()
                        },
                    ),
                ]));
                // Show mount details for the focused profile
                if selected {
                    let mount_parts: Vec<String> = profile
                        .mounts
                        .iter()
                        .map(|m| format!("{} ({})", m.path, m.usage.label()))
                        .collect();
                    lines.push(Line::from(Span::styled(
                        format!("      Mounts: {}", mount_parts.join(", ")),
                        t.tool_dim,
                    )));
                }
            }
        }

        // Filesystem layout
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "  Filesystem layout",
            t.keybind_section,
        )));

        let ws_label = format!("    /home/maki/workspace  ←  {}  (rw)", info.workspace_dir,);
        lines.push(Line::from(Span::styled(ws_label, Style::default())));

        lines.push(Line::from(Span::styled(
            "    /usr                       (ro, system)",
            t.tool_dim,
        )));
        lines.push(Line::from(Span::styled(
            "    /etc                       (tmpfs, empty)",
            t.tool_dim,
        )));
        lines.push(Line::from(Span::styled(
            "    /tmp                       (tmpfs, scratch)",
            t.tool_dim,
        )));
        lines.push(Line::from(Span::styled(
            "    /bin → /usr/bin            (symlink)",
            t.tool_dim,
        )));
        lines.push(Line::from(Span::styled(
            "    /sbin → /usr/sbin          (symlink)",
            t.tool_dim,
        )));
        lines.push(Line::from(Span::styled(
            "    /lib → /usr/lib            (symlink)",
            t.tool_dim,
        )));
        lines.push(Line::from(Span::styled(
            "    /lib64 → /usr/lib64        (symlink)",
            t.tool_dim,
        )));

        // Mounts from enabled profiles
        for (profile, enabled) in &info.profiles {
            if !enabled {
                continue;
            }
            for mount in &profile.mounts {
                if mount.usage == MountUsage::OnlyPath {
                    continue;
                }
                let name = mount.dir_name();
                lines.push(Line::from(Span::styled(
                    format!(
                        "    /home/maki/{name}  ←  {}  ({})  [{}]",
                        mount.path,
                        mount.usage.label(),
                        profile.name
                    ),
                    Style::default(),
                )));
            }
        }

        // Home directory mounts
        if !info.home_mounts.is_empty() {
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                "  Home directory mounts (rw)",
                t.keybind_section,
            )));
            for (host, name) in &info.home_mounts {
                let label = format!("    /home/maki/{name}  ←  {host}");
                lines.push(Line::from(Span::styled(label, Style::default())));
            }
        }

        // Extra workspace directories
        if !info.extra_workspace_dirs.is_empty() {
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                "  Extra workspace directories (rw)",
                t.keybind_section,
            )));
            for (host, name) in &info.extra_workspace_dirs {
                let ws = &info.workspace_name;
                let label = format!("    /home/maki/workspace/{ws}/{name}  ←  {host}");
                lines.push(Line::from(Span::styled(label, Style::default())));
            }
        }

        // Environment variables
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "  Environment variables (allow list)",
            t.keybind_section,
        )));
        for entry in &info.env_entries {
            let display_val = if entry.value.is_empty() {
                String::new()
            } else {
                format!(" = \"{}\"", entry.value)
            };
            let label = if entry.description.is_empty() {
                format!("    {}{display_val}", entry.key)
            } else {
                format!("    {}{display_val}  —  {}", entry.key, entry.description)
            };
            lines.push(Line::from(Span::styled(label, Style::default())));
        }

        // Show PATH directories contributed by enabled profiles
        let path_dirs: Vec<String> = info
            .profiles
            .iter()
            .filter(|(_, enabled)| *enabled)
            .flat_map(|(profile, _)| &profile.mounts)
            .filter(|m| m.usage == MountUsage::OnlyPath)
            .map(|m| m.sandbox_internal_path())
            .collect();
        if !path_dirs.is_empty() {
            lines.push(Line::from(Span::styled(
                format!(
                    "    (extended by active profiles: {})",
                    path_dirs.join(", ")
                ),
                t.tool_dim,
            )));
        }

        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "  Not passed to sandbox (private)",
            t.keybind_section,
        )));
        lines.push(Line::from(Span::styled(
            "    API keys, SSH tokens, GITHUB_TOKEN, AWS_* — everything not on the allow list above",
            t.tool_dim,
        )));
    }

    fn render_browse(&mut self, lines: &mut Vec<Line>, viewport_h: u16) {
        let t = theme::current();
        let Some(browser) = &mut self.browser else {
            let msg = self
                .spawn_error
                .as_deref()
                .unwrap_or("sandbox not available");
            lines.push(Line::from(Span::styled(format!("  ({msg})"), t.tool_error)));
            return;
        };

        let chrome: u16 = 2;
        browser.viewport_entries = viewport_h.saturating_sub(chrome) as usize;

        if let Some(ref err) = browser.error {
            lines.push(Line::from(Span::styled(
                format!("  error: {err}"),
                t.tool_error,
            )));
        }

        for (i, entry) in browser.visible_entries().iter().enumerate() {
            let idx = browser.scroll + i;
            let selected = idx == browser.cursor;
            let prefix = if selected { "  ▸ " } else { "    " };
            let name = if entry.name == ".." {
                "../".to_string()
            } else if entry.is_dir {
                format!("{}/", entry.name)
            } else {
                entry.name.clone()
            };
            let style = if selected {
                t.item_selected
            } else if entry.is_dir {
                Style::default()
            } else {
                t.tool_dim
            };
            lines.push(Line::from(Span::styled(format!("{prefix}{name}"), style)));
        }
    }

    fn render_shell(&mut self, lines: &mut Vec<Line>) {
        let t = theme::current();
        let Some(shell) = &self.shell else {
            let msg = self
                .spawn_error
                .as_deref()
                .unwrap_or("sandbox not available");
            lines.push(Line::from(Span::styled(format!("  ({msg})"), t.tool_error)));
            return;
        };
        for entry in &shell.entries {
            let prompt = format!("  {}:$ ", shell.cwd);
            lines.push(Line::from(vec![
                Span::styled(prompt, t.keybind_section),
                Span::styled(entry.command.clone(), Style::default()),
            ]));
            if !entry.output.is_empty() {
                let style = if entry.is_error {
                    t.tool_error
                } else {
                    Style::default()
                };
                for line in entry.output.lines() {
                    lines.push(Line::from(Span::styled(format!("    {line}"), style)));
                }
            }
            lines.push(Line::default());
        }
        if let Some(ref err) = shell.error {
            lines.push(Line::from(Span::styled(
                format!("  error: {err}"),
                t.tool_error,
            )));
            lines.push(Line::default());
        }
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        if !self.open {
            return Rect::default();
        }

        let t = theme::current();

        let max_h = (area.height as u32 * MAX_HEIGHT_PERCENT as u32 / 100)
            .max(CHROME_LINES as u32 + 1) as u16;
        let viewport_h = max_h.saturating_sub(CHROME_LINES);

        let mut lines: Vec<Line> = Vec::new();

        match self.mode {
            Mode::Info => {
                self.render_info(&mut lines);
                lines.push(Line::default());
                let tab_hint = if self.info.enabled {
                    "Tab: Browse files"
                } else {
                    "Tab: Browse (enable sandbox_enabled in config first)"
                };
                lines.push(Line::from(Span::styled(tab_hint, t.keybind_desc)));
            }
            Mode::Browse => {
                self.render_browse(&mut lines, viewport_h.saturating_sub(1));
                lines.insert(0, Line::default());
                lines.push(Line::default());
                lines.push(Line::from(Span::styled(
                    "  Tab: Info  |  Esc: Close  |  Enter: Open dir  |  Backspace: Up",
                    t.keybind_desc,
                )));
            }
            Mode::Shell => {
                self.render_shell(&mut lines);
            }
        }

        // For Shell mode we need the inner area to know input dimensions,
        // so render modal early.
        let modal = Modal {
            title: TITLE,
            width_percent: WIDTH_PERCENT,
            max_height_percent: MAX_HEIGHT_PERCENT,
        };
        let modal_lines = if self.mode == Mode::Shell {
            // Enforce minimum so the content area (split from inner) has ~10 visible lines.
            // The inner area is split into content + hints + input, so modal_lines >= 13
            // gives content >= 10 (assuming max_h isn't too constrained).
            lines.len().saturating_sub(1).max(13) as u16
        } else {
            lines.len() as u16
        };
        let (popup, inner) = modal.render(frame, area, modal_lines);

        match self.mode {
            Mode::Browse => {
                let [header_area, content_area] =
                    Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(inner);

                let cwd = self.browser.as_ref().map(|b| b.cwd.as_str()).unwrap_or("/");
                let header = format!("  sandbox:{cwd}");
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(&header, t.keybind_section))),
                    header_area,
                );

                self.scroll
                    .update_dimensions(lines.len() as u16, content_area.height);
                let scroll = self.scroll.offset();
                frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), content_area);

                let scroll_total = self
                    .browser
                    .as_ref()
                    .map(|b| b.total_entries())
                    .unwrap_or(0);
                let scroll_pos = self
                    .browser
                    .as_ref()
                    .map(|b| b.scroll_offset())
                    .unwrap_or(0);
                if scroll_total > content_area.height as usize {
                    render_vertical_scrollbar(
                        frame,
                        content_area,
                        scroll_total as u16,
                        scroll_pos as u16,
                    );
                }
            }
            Mode::Info => {
                let total = lines.len() as u16;
                self.scroll.update_dimensions(total, viewport_h);
                let scroll = self.scroll.offset();
                frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), inner);
                let scroll_pos = scroll as usize;
                if total as usize > viewport_h as usize {
                    render_vertical_scrollbar(frame, inner, total, scroll_pos as u16);
                }
            }
            Mode::Shell => {
                let [content_area, input_area, hints_area] = Layout::vertical([
                    Constraint::Fill(1),
                    Constraint::Length(1),
                    Constraint::Length(1),
                ])
                .areas(inner);

                let total = lines.len() as u16;
                self.scroll.update_dimensions(total, content_area.height);
                // Auto-scroll to bottom when new entries appear
                if let Some(s) = &self.shell
                    && s.entries.len() > self.shell_entry_count
                {
                    self.scroll.scroll_to_bottom();
                    self.shell_entry_count = s.entries.len();
                }
                let scroll = self.scroll.offset();
                frame.render_widget(
                    Paragraph::new(lines).scroll((scroll, self.h_scroll as u16)),
                    content_area,
                );

                let scroll_pos = scroll as usize;
                if total as usize > content_area.height as usize {
                    render_vertical_scrollbar(frame, content_area, total, scroll_pos as u16);
                }

                // Fixed key hints line between content and input
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(
                        "  Tab: Browse  |  Esc: Close  |  Enter: Run  |  Up/Down: History  |  Left/Right: Scroll",
                        t.keybind_desc,
                    ))),
                    hints_area,
                );

                // Input line at bottom
                let prompt = match &self.shell {
                    Some(s) => format!("  {}:$ ", s.cwd),
                    None => "  sandbox:$ ".into(),
                };
                let input_content = match &self.shell {
                    Some(s) => s.input.clone(),
                    None => String::new(),
                };
                let input_line = Line::from(vec![
                    Span::styled(prompt, t.keybind_section),
                    Span::styled(input_content, Style::default()),
                ]);
                frame.render_widget(Paragraph::new(input_line), input_area);
            }
        }

        popup
    }
}

impl Overlay for SandboxModal {
    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        SandboxModal::close(self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::key as key_ev;
    use crossterm::event::KeyCode;
    use test_case::test_case;

    fn test_info() -> SandboxInfo {
        SandboxInfo {
            enabled: false,
            env_entries: vec![],
            workspace_dir: "/tmp".into(),
            workspace_name: "tmp".into(),
            home_mounts: vec![],
            profiles: vec![],
            extra_workspace_dirs: vec![],
        }
    }

    #[test_case(key_ev(KeyCode::Esc) ; "esc_closes")]
    fn handle_key_closes(k: KeyEvent) {
        let mut modal = SandboxModal::new(test_info(), None);
        modal.toggle();
        assert!(modal.handle_key(k));
        assert!(!modal.is_open());
    }

    #[test]
    fn handle_key_consumes_all() {
        let mut modal = SandboxModal::new(test_info(), None);
        modal.toggle();
        assert!(modal.handle_key(key_ev(KeyCode::Char('a'))));
        assert!(modal.is_open());
    }

    #[test]
    fn tab_toggles_mode() {
        let mut modal = SandboxModal::new(test_info(), None);
        modal.toggle();
        assert_eq!(modal.mode, Mode::Info);
        modal.handle_key(key_ev(KeyCode::Tab));
        // Sandbox disabled, so should stay in Info mode
        assert_eq!(
            modal.mode,
            Mode::Info,
            "should stay in info when sandbox disabled"
        );
    }

    #[test]
    fn tab_switches_to_browse_when_enabled() {
        let mut modal = SandboxModal::new(
            SandboxInfo {
                enabled: true,
                env_entries: vec![],
                workspace_dir: "/tmp".into(),
                workspace_name: "tmp".into(),
                home_mounts: vec![],
                profiles: vec![],
                extra_workspace_dirs: vec![],
            },
            None,
        );
        modal.toggle();
        modal.handle_key(key_ev(KeyCode::Tab));
        assert_eq!(modal.mode, Mode::Browse);
    }

    #[test]
    fn tab_cycles_through_modes() {
        let mut modal = SandboxModal::new(
            SandboxInfo {
                enabled: true,
                env_entries: vec![],
                workspace_dir: "/tmp".into(),
                workspace_name: "tmp".into(),
                home_mounts: vec![],
                profiles: vec![],
                extra_workspace_dirs: vec![],
            },
            None,
        );
        modal.toggle();
        assert_eq!(modal.mode, Mode::Info);
        // Info -> Browse
        modal.handle_key(key_ev(KeyCode::Tab));
        assert_eq!(modal.mode, Mode::Browse);
        // Browse -> Shell
        modal.handle_key(key_ev(KeyCode::Tab));
        assert_eq!(modal.mode, Mode::Shell);
        // Shell -> Info
        modal.handle_key(key_ev(KeyCode::Tab));
        assert_eq!(modal.mode, Mode::Info);
    }
}
