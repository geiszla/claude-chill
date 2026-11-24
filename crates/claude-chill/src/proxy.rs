use crate::escape_sequences::{
    ALT_SCREEN_ENTER, ALT_SCREEN_ENTER_LEGACY, ALT_SCREEN_EXIT, ALT_SCREEN_EXIT_LEGACY,
    BRACKETED_PASTE_DISABLE, BRACKETED_PASTE_ENABLE, BRACKETED_PASTE_END, BRACKETED_PASTE_START,
    CLEAR_SCROLLBACK, CURSOR_HOME, ERASE_BELOW, INPUT_BUFFER_CAPACITY, OUTPUT_BUFFER_CAPACITY,
    SYNC_END, SYNC_START,
};
use anyhow::{Context, Result};
use log::debug;
use memchr::memmem;
use nix::errno::Errno;
use nix::fcntl::{FcntlArg, OFlag, fcntl};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::pty::{Winsize, openpty};
use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, kill, sigaction};
use nix::sys::termios::{SetArg, Termios, cfmakeraw, tcgetattr, tcsetattr};
use nix::unistd::{Pid, isatty, read, write};
use std::io;
use std::num::NonZeroU16;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use termwiz::escape::Action;
use termwiz::escape::csi::{CSI, Keyboard};
use termwiz::escape::parser::Parser as TermwizParser;

static SIGWINCH_RECEIVED: AtomicBool = AtomicBool::new(false);
static SIGINT_RECEIVED: AtomicBool = AtomicBool::new(false);
static SIGTERM_RECEIVED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SequenceMatch {
    Complete,
    Partial,
    None,
}

extern "C" fn handle_sigwinch(_: libc::c_int) {
    SIGWINCH_RECEIVED.store(true, Ordering::SeqCst);
}

extern "C" fn handle_sigint(_: libc::c_int) {
    SIGINT_RECEIVED.store(true, Ordering::SeqCst);
}

extern "C" fn handle_sigterm(_: libc::c_int) {
    SIGTERM_RECEIVED.store(true, Ordering::SeqCst);
}

pub struct ProxyConfig {
    pub max_history_lines: usize,
    pub lookback_key: String,
    pub lookback_sequence_legacy: Vec<u8>,
    pub lookback_sequence_kitty: Vec<u8>,
    pub auto_lookback_timeout_ms: u64,
    pub resize_refresh_ms: u64,
    pub startup_refresh_ms: u64,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            max_history_lines: 100_000,
            lookback_key: "[ctrl][6]".to_string(),
            lookback_sequence_legacy: vec![0x1E],
            lookback_sequence_kitty: b"\x1b[54;5u".to_vec(),
            auto_lookback_timeout_ms: 15000,
            resize_refresh_ms: 500,
            startup_refresh_ms: 0,
        }
    }
}

struct TerminalGuard {
    original_termios: Option<Termios>,
}

impl TerminalGuard {
    fn new() -> Result<Self> {
        let original_termios = setup_raw_mode()?;
        Ok(Self { original_termios })
    }

    fn take(mut self) -> Option<Termios> {
        self.original_termios.take()
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if let Some(ref termios) = self.original_termios {
            let _ = tcsetattr(io::stdin(), SetArg::TCSANOW, termios);
        }
    }
}

const RENDER_DELAY_MS: u64 = 5;
const SYNC_BLOCK_DELAY_MS: u64 = 50;
const RESIZE_DEBOUNCE_MS: u64 = 150;
const RESIZE_REFRESH_MAX_WAIT_MS: u64 = 1500;
// A lone Esc is a prefix of an Esc-based lookback key; forward it after this long
const LOOKBACK_HOLD_MS: u64 = 50;

pub struct Proxy {
    config: ProxyConfig,
    pty_master: OwnedFd,
    child: Child,
    original_termios: Option<Termios>,
    vt_parser: vt100::Parser,
    /// What the terminal shows: the live output is fed to it as it is written
    shadow: vt100::Parser,
    force_full_render: bool,
    scrollback_rows_at_render: usize,
    reprint_dump_pending: bool,
    resize_changed_width: bool,
    last_output_time: Option<Instant>,
    last_render_time: Option<Instant>,
    last_stdin_time: Option<Instant>,
    last_auto_lookback_time: Option<Instant>,
    auto_lookback_timeout: Duration,
    winch_pending_since: Option<Instant>,
    resize_refresh_due: Option<Instant>,
    resize_refresh_delay: Duration,
    startup_refresh_quiet: Duration,
    startup_refresh_pending: bool,
    scrollback_rows_seen: usize,
    last_scrollback_growth: Option<Instant>,
    in_sync_block: bool,
    in_lookback_mode: bool,
    in_alternate_screen: bool,
    in_bracketed_paste: bool,
    kitty_mode_supported: bool,
    kitty_mode_stack: u32,
    kitty_output_parser: TermwizParser,
    vt_render_pending: bool,
    lookback_cache: Vec<u8>,
    lookback_input_buffer: Vec<u8>,
    lookback_hold_since: Option<Instant>,
    output_buffer: Vec<u8>,
    sync_start_finder: memmem::Finder<'static>,
    sync_end_finder: memmem::Finder<'static>,
    alt_screen_enter_finder: memmem::Finder<'static>,
    alt_screen_exit_finder: memmem::Finder<'static>,
    alt_screen_enter_legacy_finder: memmem::Finder<'static>,
    alt_screen_exit_legacy_finder: memmem::Finder<'static>,
    paste_start_finder: memmem::Finder<'static>,
    paste_end_finder: memmem::Finder<'static>,
    pty_drain_buffer: Vec<u8>,
    paste_remainder: Vec<u8>,
}

/// Returns (supported, initial_flags) - if flags > 0, terminal is already in Kitty mode
fn detect_kitty_support() -> (bool, u32) {
    use std::io::Write;
    use termwiz::escape::csi::Device;

    // Query sequences:
    // CSI ? u       - Kitty keyboard protocol query
    // CSI c         - Primary Device Attributes (all terminals respond)
    const KITTY_QUERY: &[u8] = b"\x1b[?u";
    const DA_QUERY: &[u8] = b"\x1b[c";

    // Send both queries
    let stdout = std::io::stdout();
    let mut stdout_lock = stdout.lock();
    if stdout_lock.write_all(KITTY_QUERY).is_err() {
        return (false, 0);
    }
    if stdout_lock.write_all(DA_QUERY).is_err() {
        return (false, 0);
    }
    if stdout_lock.flush().is_err() {
        return (false, 0);
    }
    drop(stdout_lock);

    // Read responses with timeout using termwiz parser
    let stdin = std::io::stdin();
    let mut parser = TermwizParser::new();
    let mut buf = [0u8; 256];
    let mut kitty_supported = false;
    let mut kitty_flags: u32 = 0;
    let start = std::time::Instant::now();
    let timeout = std::time::Duration::from_millis(500);
    let poll_interval = PollTimeout::from(50u16);

    while start.elapsed() < timeout {
        let mut poll_fd = [PollFd::new(stdin.as_fd(), PollFlags::POLLIN)];

        match poll(&mut poll_fd, poll_interval) {
            Ok(0) => continue,
            Ok(_) => {
                match read(stdin.as_fd(), &mut buf) {
                    Ok(0) => continue,
                    Ok(n) => {
                        let actions = parser.parse_as_vec(&buf[..n]);
                        for action in actions {
                            if let Action::CSI(csi) = action {
                                match csi {
                                    CSI::Keyboard(Keyboard::ReportKittyState(flags)) => {
                                        kitty_supported = true;
                                        kitty_flags = u32::from(flags.bits());
                                    }
                                    CSI::Device(dev)
                                        if matches!(*dev, Device::DeviceAttributes(_)) =>
                                    {
                                        // DA response means all responses received
                                        debug!(
                                            "Kitty detection complete: supported={} flags={}",
                                            kitty_supported, kitty_flags
                                        );
                                        return (kitty_supported, kitty_flags);
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                    Err(e) if e == Errno::EAGAIN || e == Errno::EWOULDBLOCK => continue,
                    Err(_) => break,
                }
            }
            Err(_) => continue,
        }
    }

    debug!(
        "Kitty detection timed out, supported={} flags={}",
        kitty_supported, kitty_flags
    );
    (kitty_supported, kitty_flags)
}

impl Proxy {
    pub fn spawn(command: &str, args: &[&str], config: ProxyConfig) -> Result<Self> {
        let winsize = get_terminal_size()?;
        let pty = openpty(&winsize, None).context("openpty failed")?;

        let terminal_guard = TerminalGuard::new()?;
        setup_signal_handlers()?;

        // Detect Kitty support before spawning child
        // If flags > 0, terminal is already in Kitty mode (inherited from parent)
        let (kitty_supported, kitty_initial_flags) = detect_kitty_support();
        let kitty_initial_stack = if kitty_initial_flags > 0 { 1 } else { 0 };

        let slave_fd = pty.slave.as_raw_fd();

        let child = unsafe {
            Command::new(command)
                .args(args)
                .pre_exec(move || {
                    if libc::setsid() == -1 {
                        return Err(io::Error::last_os_error());
                    }
                    if libc::ioctl(slave_fd, libc::TIOCSCTTY as libc::c_ulong, 0) == -1 {
                        return Err(io::Error::last_os_error());
                    }
                    if libc::dup2(slave_fd, 0) == -1 {
                        return Err(io::Error::last_os_error());
                    }
                    if libc::dup2(slave_fd, 1) == -1 {
                        return Err(io::Error::last_os_error());
                    }
                    if libc::dup2(slave_fd, 2) == -1 {
                        return Err(io::Error::last_os_error());
                    }
                    if slave_fd > 2 {
                        libc::close(slave_fd);
                    }
                    Ok(())
                })
                .spawn()
                .context("spawn failed")?
        };

        drop(pty.slave);
        set_nonblocking(&pty.master)?;

        // Enable bracketed paste on the real terminal so tmux/Ghostty wraps
        // paste content in \x1b[200~ ... \x1b[201~ markers. Without this,
        // the child app's \x1b[?2004h gets eaten by the VT renderer before
        // it reaches the terminal, and pastes arrive as plain keystrokes.
        let stdout_fd = io::stdout();
        write_all(&stdout_fd, BRACKETED_PASTE_ENABLE)?;

        let vt_parser = vt100::Parser::new(
            nonzero(winsize.ws_row),
            nonzero(winsize.ws_col),
            config.max_history_lines,
        );

        // Seed history with clear screen so replay starts fresh
        let auto_lookback_timeout = Duration::from_millis(config.auto_lookback_timeout_ms);
        let resize_refresh_delay = Duration::from_millis(config.resize_refresh_ms);
        let startup_refresh_quiet = Duration::from_millis(config.startup_refresh_ms);

        debug!("Proxy::spawn: command={} args={:?}", command, args);

        Ok(Self {
            config,
            pty_master: pty.master,
            child,
            original_termios: terminal_guard.take(),
            vt_parser,
            shadow: vt100::Parser::new(nonzero(winsize.ws_row), nonzero(winsize.ws_col), 0),
            force_full_render: true,
            scrollback_rows_at_render: 0,
            reprint_dump_pending: false,
            resize_changed_width: false,
            last_output_time: None,
            last_render_time: None,
            last_stdin_time: None,
            last_auto_lookback_time: None,
            auto_lookback_timeout,
            winch_pending_since: None,
            resize_refresh_due: None,
            resize_refresh_delay,
            startup_refresh_quiet,
            startup_refresh_pending: !startup_refresh_quiet.is_zero(),
            scrollback_rows_seen: 0,
            last_scrollback_growth: None,
            in_sync_block: false,
            in_lookback_mode: false,
            in_alternate_screen: false,
            in_bracketed_paste: false,
            kitty_mode_supported: kitty_supported,
            kitty_mode_stack: kitty_initial_stack,
            kitty_output_parser: TermwizParser::new(),
            vt_render_pending: false,
            lookback_cache: Vec::new(),
            lookback_input_buffer: Vec::with_capacity(INPUT_BUFFER_CAPACITY),
            lookback_hold_since: None,
            output_buffer: Vec::with_capacity(OUTPUT_BUFFER_CAPACITY),
            sync_start_finder: memmem::Finder::new(SYNC_START),
            sync_end_finder: memmem::Finder::new(SYNC_END),
            alt_screen_enter_finder: memmem::Finder::new(ALT_SCREEN_ENTER),
            alt_screen_exit_finder: memmem::Finder::new(ALT_SCREEN_EXIT),
            alt_screen_enter_legacy_finder: memmem::Finder::new(ALT_SCREEN_ENTER_LEGACY),
            alt_screen_exit_legacy_finder: memmem::Finder::new(ALT_SCREEN_EXIT_LEGACY),
            paste_start_finder: memmem::Finder::new(BRACKETED_PASTE_START),
            paste_end_finder: memmem::Finder::new(BRACKETED_PASTE_END),
            pty_drain_buffer: Vec::new(),
            paste_remainder: Vec::new(),
        })
    }

    pub fn run(&mut self) -> Result<i32> {
        let stdin_fd = io::stdin();
        let stdout_fd = io::stdout();

        let mut buf = [0u8; 65536];

        loop {
            if SIGWINCH_RECEIVED.swap(false, Ordering::SeqCst) {
                self.winch_pending_since = Some(Instant::now());
            }
            // A drag fires many resizes. The child only gets the settled size, so it
            // repaints once and leaves one seam in the history instead of one per event.
            if let Some(since) = self.winch_pending_since
                && since.elapsed() >= Duration::from_millis(RESIZE_DEBOUNCE_MS)
            {
                self.winch_pending_since = None;
                self.forward_winsize()?;
                if !self.resize_refresh_delay.is_zero() {
                    self.resize_refresh_due = Some(Instant::now() + self.resize_refresh_delay);
                }
            }
            if SIGINT_RECEIVED.swap(false, Ordering::SeqCst) {
                self.forward_signal(Signal::SIGINT);
            }
            self.flush_held_input()?;
            if SIGTERM_RECEIVED.swap(false, Ordering::SeqCst) {
                self.forward_signal(Signal::SIGTERM);
            }

            let master_fd = unsafe { BorrowedFd::borrow_raw(self.pty_master.as_raw_fd()) };
            let stdin_borrowed = unsafe { BorrowedFd::borrow_raw(stdin_fd.as_raw_fd()) };

            let mut poll_fds = [
                PollFd::new(master_fd, PollFlags::POLLIN),
                PollFd::new(stdin_borrowed, PollFlags::POLLIN),
            ];

            let mut poll_timeout_ms = self
                .time_until_render()
                .map(|d| d.as_millis().min(100) as u16)
                .unwrap_or(100);
            if let Some(since) = self.lookback_hold_since {
                let remaining =
                    Duration::from_millis(LOOKBACK_HOLD_MS).saturating_sub(since.elapsed());
                poll_timeout_ms = poll_timeout_ms.min(remaining.as_millis().max(1) as u16);
            }

            match poll(&mut poll_fds, PollTimeout::from(poll_timeout_ms)) {
                Ok(0) => {
                    self.flush_pending_vt_render(&stdout_fd)?;
                    self.check_auto_lookback(&stdout_fd)?;
                    self.check_scrollback_refresh(&stdout_fd)?;
                    continue;
                }
                Ok(_) => {}
                Err(Errno::EINTR) => continue,
                Err(e) => anyhow::bail!("poll failed: {}", e),
            }

            self.flush_pending_vt_render(&stdout_fd)?;
            self.check_scrollback_refresh(&stdout_fd)?;

            if let Some(revents) = poll_fds[0].revents() {
                if revents.contains(PollFlags::POLLIN) {
                    match nix_read(&self.pty_master, &mut buf) {
                        Ok(0) => break,
                        Ok(n) => self.process_output(&buf[..n], &stdout_fd)?,
                        Err(Errno::EAGAIN) => {}
                        Err(Errno::EIO) => break,
                        Err(e) => anyhow::bail!("read from pty failed: {}", e),
                    }
                }
                if revents.contains(PollFlags::POLLHUP) {
                    break;
                }
            }

            if let Some(revents) = poll_fds[1].revents()
                && revents.contains(PollFlags::POLLIN)
            {
                match nix_read(&stdin_fd, &mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        self.process_input(&buf[..n], &stdout_fd)?;
                        if !self.pty_drain_buffer.is_empty() {
                            let drained = std::mem::take(&mut self.pty_drain_buffer);
                            self.process_output(&drained, &stdout_fd)?;
                        }
                    }
                    Err(Errno::EAGAIN) => {}
                    Err(e) => anyhow::bail!("read from stdin failed: {}", e),
                }
            }
        }

        // Final render before exit
        if self.vt_render_pending {
            self.render_vt_screen(&stdout_fd)?;
        }

        self.wait_child()
    }

    fn process_output<F: AsFd>(&mut self, data: &[u8], stdout_fd: &F) -> Result<()> {
        self.process_output_inner(data, stdout_fd, true)
    }

    fn process_output_inner<F: AsFd>(
        &mut self,
        data: &[u8],
        stdout_fd: &F,
        feed_vt: bool,
    ) -> Result<()> {
        debug!(
            "process_output: len={} in_alt={} in_lookback={} feed_vt={}",
            data.len(),
            self.in_alternate_screen,
            self.in_lookback_mode,
            feed_vt
        );

        if self.in_alternate_screen {
            // Feed VT but NOT history while in alt screen
            // Alt screen content (TUI editors, etc.) shouldn't be in lookback history
            if feed_vt {
                self.vt_parser.process(data);
            }
            return self.process_output_alt_screen(data, stdout_fd);
        }

        if self.in_lookback_mode {
            debug!("process_output: caching {} bytes for lookback", data.len());
            self.lookback_cache.extend_from_slice(data);
            return Ok(());
        }

        // Feed data to VT emulator (unless already fed by caller)
        if feed_vt {
            self.vt_parser.process(data);
        }
        self.vt_render_pending = true;
        self.last_output_time = Some(Instant::now());
        if self.startup_refresh_pending {
            let rows = self.scrollback_rows();
            if rows > self.scrollback_rows_seen {
                self.scrollback_rows_seen = rows;
                self.last_scrollback_growth = Some(Instant::now());
            }
        }

        // Process sync blocks for history management
        let mut pos = 0;
        while pos < data.len() {
            // Check for alt screen enter
            if let Some(alt_pos) = self.find_alt_screen_enter(&data[pos..]) {
                debug!(
                    "process_output: ALT_SCREEN_ENTER detected at pos={}",
                    pos + alt_pos
                );
                self.in_sync_block = false;
                self.in_alternate_screen = true;
                let seq_len = self.alt_screen_enter_len(&data[pos + alt_pos..]);
                // Write alt screen enter directly
                self.write_to_terminal(stdout_fd, &data[pos + alt_pos..pos + alt_pos + seq_len])?;
                return self.process_output_alt_screen(&data[pos + alt_pos + seq_len..], stdout_fd);
            }

            if self.in_sync_block {
                if let Some(idx) = self.sync_end_finder.find(&data[pos..]) {
                    debug!("process_output: SYNC_END at pos={}", pos + idx);
                    self.in_sync_block = false;
                    pos += idx + SYNC_END.len();
                } else {
                    break;
                }
            } else if let Some(idx) = self.sync_start_finder.find(&data[pos..]) {
                debug!("process_output: SYNC_START at pos={}", pos + idx);
                self.in_sync_block = true;
                pos += idx + SYNC_START.len();
            } else {
                break;
            }
        }

        Ok(())
    }

    fn process_output_alt_screen<F: AsFd>(&mut self, data: &[u8], stdout_fd: &F) -> Result<()> {
        if let Some(exit_pos) = self.find_alt_screen_exit(data) {
            debug!(
                "process_output_alt_screen: ALT_SCREEN_EXIT detected at pos={}",
                exit_pos
            );
            self.write_to_terminal(stdout_fd, &data[..exit_pos])?;
            let seq_len = self.alt_screen_exit_len(&data[exit_pos..]);
            self.write_to_terminal(stdout_fd, &data[exit_pos..exit_pos + seq_len])?;
            self.in_alternate_screen = false;

            // Force full VT render to restore main screen content
            debug!("process_output_alt_screen: rendering VT screen after alt exit");
            self.force_full_render = true;
            self.render_vt_screen(stdout_fd)?;

            // Data after ALT_EXIT was already fed to VT and history when we processed
            // the alt screen chunk, so we just need to check for more alt screen transitions
            let remaining = &data[exit_pos + seq_len..];
            if !remaining.is_empty() {
                // Check if there's another alt screen enter in the remaining data
                if self.find_alt_screen_enter(remaining).is_some() {
                    // Need to process for alt screen detection, but skip VT/history feed
                    return self.process_output_check_alt_only(remaining, stdout_fd);
                }
            }
            return Ok(());
        }
        self.write_to_terminal(stdout_fd, data)
    }

    /// Check for alt screen transitions without re-feeding VT/history
    fn process_output_check_alt_only<F: AsFd>(&mut self, data: &[u8], stdout_fd: &F) -> Result<()> {
        if let Some(alt_pos) = self.find_alt_screen_enter(data) {
            debug!(
                "process_output_check_alt_only: ALT_SCREEN_ENTER at pos={}",
                alt_pos
            );
            self.in_alternate_screen = true;
            let seq_len = self.alt_screen_enter_len(&data[alt_pos..]);
            self.write_to_terminal(stdout_fd, &data[alt_pos..alt_pos + seq_len])?;
            return self.process_output_alt_screen(&data[alt_pos + seq_len..], stdout_fd);
        }
        Ok(())
    }

    fn find_alt_screen_enter(&self, data: &[u8]) -> Option<usize> {
        let pos1 = self.alt_screen_enter_finder.find(data);
        let pos2 = self.alt_screen_enter_legacy_finder.find(data);
        match (pos1, pos2) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        }
    }

    fn find_alt_screen_exit(&self, data: &[u8]) -> Option<usize> {
        let pos1 = self.alt_screen_exit_finder.find(data);
        let pos2 = self.alt_screen_exit_legacy_finder.find(data);
        match (pos1, pos2) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        }
    }

    fn alt_screen_enter_len(&self, data: &[u8]) -> usize {
        if data.starts_with(ALT_SCREEN_ENTER) {
            ALT_SCREEN_ENTER.len()
        } else {
            ALT_SCREEN_ENTER_LEGACY.len()
        }
    }

    fn alt_screen_exit_len(&self, data: &[u8]) -> usize {
        if data.starts_with(ALT_SCREEN_EXIT) {
            ALT_SCREEN_EXIT.len()
        } else {
            ALT_SCREEN_EXIT_LEGACY.len()
        }
    }

    fn kitty_mode_enabled(&self) -> bool {
        self.kitty_mode_stack > 0
    }

    /// Write data to the terminal and track Kitty keyboard protocol state
    fn write_to_terminal<F: AsFd>(&mut self, stdout_fd: &F, data: &[u8]) -> Result<()> {
        Self::update_kitty_mode_helper(
            &mut self.kitty_output_parser,
            &mut self.kitty_mode_stack,
            self.kitty_mode_supported,
            data,
        );
        write_all(stdout_fd, data)
    }

    fn update_kitty_mode_helper(
        parser: &mut TermwizParser,
        stack: &mut u32,
        supported: bool,
        data: &[u8],
    ) {
        let actions = parser.parse_as_vec(data);
        for action in actions {
            if let Action::CSI(csi) = action {
                match csi {
                    CSI::Keyboard(Keyboard::PushKittyState { flags, .. }) => {
                        if supported {
                            *stack = stack.saturating_add(1);
                            debug!(
                                "Kitty keyboard protocol push (flags={:?}, stack={})",
                                flags, stack
                            );
                        }
                    }
                    CSI::Keyboard(Keyboard::SetKittyState { flags, .. }) => {
                        if supported && !flags.is_empty() && *stack == 0 {
                            *stack = 1;
                            debug!(
                                "Kitty keyboard protocol set (flags={:?}, stack={})",
                                flags, stack
                            );
                        } else if flags.is_empty() && *stack > 0 {
                            debug!("Kitty keyboard protocol set empty flags (stack={})", stack);
                        }
                    }
                    CSI::Keyboard(Keyboard::PopKittyState(n)) => {
                        let prev = *stack;
                        *stack = stack.saturating_sub(n);
                        debug!(
                            "Kitty keyboard protocol pop {} (stack {} -> {})",
                            n, prev, stack
                        );
                    }
                    _ => {}
                }
            }
        }
    }

    fn flush_pending_vt_render<F: AsFd>(&mut self, stdout_fd: &F) -> Result<()> {
        if !self.vt_render_pending || self.in_lookback_mode || self.in_alternate_screen {
            return Ok(());
        }

        let elapsed = self
            .last_output_time
            .map(|t| t.elapsed())
            .unwrap_or(Duration::MAX);

        // Wait longer if in sync block (more data likely coming)
        let delay = if self.in_sync_block {
            Duration::from_millis(SYNC_BLOCK_DELAY_MS)
        } else {
            Duration::from_millis(RENDER_DELAY_MS)
        };

        if elapsed >= delay {
            if self.reprint_dump_pending {
                self.reprint_dump_pending = false;
                return self.dump_history(stdout_fd);
            }
            self.render_vt_screen(stdout_fd)?;
        }

        Ok(())
    }

    fn time_until_render(&self) -> Option<Duration> {
        if !self.vt_render_pending || self.in_lookback_mode || self.in_alternate_screen {
            return None;
        }

        let elapsed = self
            .last_output_time
            .map(|t| t.elapsed())
            .unwrap_or(Duration::MAX);

        let delay = if self.in_sync_block {
            Duration::from_millis(SYNC_BLOCK_DELAY_MS)
        } else {
            Duration::from_millis(RENDER_DELAY_MS)
        };

        if elapsed >= delay {
            Some(Duration::ZERO)
        } else {
            Some(delay - elapsed)
        }
    }

    fn render_vt_screen<F: AsFd>(&mut self, stdout_fd: &F) -> Result<()> {
        let (rows, cols) = self.vt_parser.screen().size();
        let scrolled = self.scrollback_rows();
        let full = self.force_full_render;
        self.output_buffer.clear();
        self.output_buffer.extend_from_slice(SYNC_START);

        if full {
            self.force_full_render = false;
            let frame = self.vt_parser.screen().contents_formatted();
            self.shadow.process(frame.as_bytes());
            self.output_buffer.extend_from_slice(frame.as_bytes());
        } else {
            // Rows that left the emulator's screen since the last render leave the
            // terminal's screen the same way, so its scrollback grows with the
            // conversation and what was above the live rows stays put. Once the
            // emulator's scrollback is full the count is unknown, so the screen is
            // repainted in place instead.
            let k = scrolled.saturating_sub(self.scrollback_rows_at_render);
            if k > 0 && scrolled < self.config.max_history_lines {
                let scroll = self.mirror_scroll(k, usize::from(rows.get()), cols.get());
                self.shadow.process(&scroll);
                self.output_buffer.extend_from_slice(&scroll);
            }
            let diff = self.vt_parser.screen().contents_diff(self.shadow.screen());
            self.shadow.process(diff.as_bytes());
            self.output_buffer.extend_from_slice(diff.as_bytes());
        }

        let cursor = self.vt_parser.screen().cursor_state_formatted();
        self.shadow.process(cursor.as_bytes());
        self.output_buffer.extend_from_slice(cursor.as_bytes());
        self.output_buffer.extend_from_slice(SYNC_END);

        debug!(
            "render_vt_screen: full={} scrolled={} output_len={}\n",
            full,
            scrolled,
            self.output_buffer.len()
        );
        // Can't use write_to_terminal here due to borrow checker - can't pass
        // &self.output_buffer while also taking &mut self
        Self::update_kitty_mode_helper(
            &mut self.kitty_output_parser,
            &mut self.kitty_mode_stack,
            self.kitty_mode_supported,
            &self.output_buffer,
        );
        write_all(stdout_fd, &self.output_buffer)?;

        self.scrollback_rows_at_render = scrolled;
        self.vt_render_pending = false;
        self.last_render_time = Some(Instant::now());
        Ok(())
    }

    /// Terminal bytes that scroll `k` rows out of the screen the way the emulator did:
    /// the leaving rows are first brought to the state they left in, then each scroll
    /// step writes the row that enters at the bottom
    fn mirror_scroll(&mut self, k: usize, n: usize, cols: u16) -> Vec<u8> {
        let total = self.scrollback_rows() + n;
        let tail = self.stream_rows(total - (k + n), cols);
        let shadow_rows: Vec<Vec<u8>> = self
            .shadow
            .screen()
            .rows_formatted(0, cols)
            .map(|row| row.strip_prefix("\r\n").unwrap_or(&row).as_bytes().to_vec())
            .collect();
        let mut out = Vec::new();
        for i in 0..k.min(n) {
            if tail[i] != shadow_rows[i] {
                out.extend_from_slice(format!("\x1b[{};1H\x1b[2K", i + 1).as_bytes());
                out.extend_from_slice(&tail[i]);
                out.extend_from_slice(b"\x1b[m");
            }
        }
        out.extend_from_slice(format!("\x1b[{n};1H").as_bytes());
        tail[n..n + k].iter().for_each(|incoming| {
            out.extend_from_slice(b"\r\n");
            if !incoming.is_empty() {
                out.extend_from_slice(incoming);
                out.extend_from_slice(b"\x1b[m");
            }
        });
        out
    }

    /// Rows the emulator holds above its screen
    fn scrollback_rows(&mut self) -> usize {
        let screen = self.vt_parser.screen_mut();
        screen.set_scrollback(usize::MAX);
        let rows = screen.scrollback();
        screen.set_scrollback(0);
        rows
    }

    /// Rewrite the terminal scrollback from the emulator so it holds one copy of the
    /// history: once after startup, when the history has grown (the transcript a
    /// resumed session prints) and then stopped growing, which a spinner never
    /// changes; and after a settled resize, once the child's repaint has gone quiet
    fn check_scrollback_refresh<F: AsFd>(&mut self, stdout_fd: &F) -> Result<()> {
        if self.in_lookback_mode
            || self.in_alternate_screen
            || self.in_sync_block
            || self.winch_pending_since.is_some()
        {
            return Ok(());
        }
        if self.startup_refresh_pending
            && let Some(grown) = self.last_scrollback_growth
            && grown.elapsed() >= self.startup_refresh_quiet
        {
            self.startup_refresh_pending = false;
            debug!("startup refresh: rewriting scrollback from the emulator");
            return self.dump_history(stdout_fd);
        }
        let Some(due) = self.resize_refresh_due else {
            return Ok(());
        };
        let now = Instant::now();
        if now < due {
            return Ok(());
        }
        // Wait for the child's repaint to go quiet, but not forever: a spinner never does
        let quiet = self
            .last_output_time
            .is_none_or(|t| t.elapsed() >= Duration::from_millis(RESIZE_DEBOUNCE_MS));
        if !quiet && now < due + Duration::from_millis(RESIZE_REFRESH_MAX_WAIT_MS) {
            return Ok(());
        }
        self.resize_refresh_due = None;
        // A height change only moves rows between the emulator's scrollback and screen.
        // A width change makes Claude Code wrap its rows again, which only it can do.
        if std::mem::take(&mut self.resize_changed_width) {
            debug!("resize refresh: asking the child to print its transcript again");
            self.request_reprint();
            return Ok(());
        }
        debug!("resize refresh: rewriting scrollback from the emulator");
        self.dump_history(stdout_fd)
    }

    /// On SIGCONT Claude Code prints its whole transcript again from the cursor: a new
    /// emulator takes that as the history, and the next render writes it to the terminal.
    fn request_reprint(&mut self) {
        let (rows, cols) = self.vt_parser.screen().size();
        self.vt_parser = vt100::Parser::new(rows, cols, self.config.max_history_lines);
        self.scrollback_rows_at_render = 0;
        self.reprint_dump_pending = true;
        self.forward_signal(Signal::SIGCONT);
    }

    fn check_auto_lookback<F: AsFd>(&mut self, stdout_fd: &F) -> Result<()> {
        if self.auto_lookback_timeout.is_zero() {
            return Ok(());
        }
        if self.in_lookback_mode || self.in_alternate_screen {
            return Ok(());
        }

        // Check if enough time has passed since last stdin activity
        let Some(stdin_time) = self.last_stdin_time else {
            return Ok(());
        };
        if stdin_time.elapsed() < self.auto_lookback_timeout {
            return Ok(());
        }

        // Check if there's been new output since last auto-lookback
        // AND enough time has passed since we last dumped
        let Some(render_time) = self.last_render_time else {
            return Ok(());
        };
        if let Some(last_auto) = self.last_auto_lookback_time {
            let no_new_output = render_time <= last_auto;
            let too_soon = last_auto.elapsed() < self.auto_lookback_timeout;
            if no_new_output || too_soon {
                return Ok(());
            }
        }

        debug!(
            "auto_lookback triggered: stdin_idle={}ms render_age={}ms last_auto_age={}ms",
            stdin_time.elapsed().as_millis(),
            render_time.elapsed().as_millis(),
            self.last_auto_lookback_time
                .map(|t| t.elapsed().as_millis())
                .unwrap_or(0)
        );
        self.dump_history(stdout_fd)?;
        self.last_auto_lookback_time = Some(Instant::now());
        Ok(())
    }

    /// Formatted rows of the emulator's scrollback followed by its screen, from the
    /// given index on. Off-screen rows keep the width they were rendered at (the
    /// emulator does not reflow); a width of `u16::MAX` emits all their cells so the
    /// terminal wraps them
    fn stream_rows(&mut self, from: usize, width: u16) -> Vec<Vec<u8>> {
        let rows = usize::from(self.vt_parser.screen().size().0.get());
        let screen = self.vt_parser.screen_mut();
        screen.set_scrollback(usize::MAX);
        let top = screen.scrollback();
        let mut out = Vec::new();
        let mut offset = top.saturating_sub(from);
        let mut next = top - offset;
        loop {
            screen.set_scrollback(offset);
            let first = top - offset;
            for (i, row) in screen.rows_formatted(0, width).enumerate() {
                let index = first + i;
                if index < next {
                    continue;
                }
                next = index + 1;
                if index >= from {
                    // A wrapped continuation row starts with its own line break
                    out.push(row.strip_prefix("\r\n").unwrap_or(&row).as_bytes().to_vec());
                }
            }
            if offset == 0 {
                break;
            }
            offset = offset.saturating_sub(rows);
        }
        screen.set_scrollback(0);
        out
    }

    /// The emulator's scrollback plus the live screen, one line per row. Claude Code
    /// repaints in place (cursor home, erase each row), so the emulator holds each row
    /// once as last rendered, unlike the raw byte history, which replays every
    /// partial repaint.
    fn history_snapshot(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        self.stream_rows(0, u16::MAX).iter().for_each(|row| {
            if !out.is_empty() {
                out.extend_from_slice(b"\r\n");
            }
            out.extend_from_slice(row);
            out.extend_from_slice(b"\x1b[m");
        });
        out
    }

    fn dump_history<F: AsFd>(&mut self, stdout_fd: &F) -> Result<()> {
        let snapshot = self.history_snapshot();
        debug!("dump_history: snapshot_bytes={}", snapshot.len());
        self.output_buffer.clear();
        self.output_buffer.extend_from_slice(&snapshot);

        // Debug: write history to file if CLAUDE_CHILL_HISTORY_FILE is set
        if let Ok(path) = std::env::var("CLAUDE_CHILL_HISTORY_FILE")
            && let Err(e) = std::fs::write(&path, &self.output_buffer)
        {
            debug!("Failed to write history file: {}", e);
        }

        // Erase the screen in place and wipe the terminal's own scrollback, so the
        // snapshot is its only copy. Not ESC[2J: VS Code's terminal scrolls the screen
        // into the scrollback on that instead of erasing it.
        self.write_to_terminal(stdout_fd, CURSOR_HOME)?;
        self.write_to_terminal(stdout_fd, ERASE_BELOW)?;
        self.write_to_terminal(stdout_fd, CLEAR_SCROLLBACK)?;
        // Can't use write_to_terminal here due to borrow checker - can't pass
        // &self.output_buffer while also taking &mut self
        Self::update_kitty_mode_helper(
            &mut self.kitty_output_parser,
            &mut self.kitty_mode_stack,
            self.kitty_mode_supported,
            &self.output_buffer,
        );
        write_all(stdout_fd, &self.output_buffer)?;

        // The terminal shows the snapshot's tail, which is the live screen; repaint it
        // in place (a full render starts with home + erase below) so later diffs start
        // from a known state
        self.force_full_render = true;
        self.render_vt_screen(stdout_fd)
    }

    fn process_input<F: AsFd>(&mut self, data: &[u8], stdout_fd: &F) -> Result<()> {
        self.last_stdin_time = Some(Instant::now());

        debug!(
            "process_input: stdin={:?} paste={}",
            data, self.in_bracketed_paste
        );

        // In alternate screen, forward directly with deadlock prevention
        if self.in_alternate_screen {
            return write_to_pty_draining(&self.pty_master, data, &mut self.pty_drain_buffer);
        }

        // In bracketed paste, forward directly until paste end marker
        if self.in_bracketed_paste {
            let combined;
            let search_data = if self.paste_remainder.is_empty() {
                data
            } else {
                combined = [self.paste_remainder.as_slice(), data].concat();
                self.paste_remainder.clear();
                &combined
            };

            if let Some(pos) = self.paste_end_finder.find(search_data) {
                let end = pos + BRACKETED_PASTE_END.len();
                write_to_pty_draining(
                    &self.pty_master,
                    &search_data[..end],
                    &mut self.pty_drain_buffer,
                )?;
                self.in_bracketed_paste = false;
                debug!("process_input: bracketed paste ended");
                if end < search_data.len() {
                    return self.process_input(&search_data[end..], stdout_fd);
                }
                return Ok(());
            }

            let (forward, remainder) =
                split_trailing_marker_prefix(search_data, BRACKETED_PASTE_END);
            if !forward.is_empty() {
                write_to_pty_draining(&self.pty_master, forward, &mut self.pty_drain_buffer)?;
            }
            self.paste_remainder.extend_from_slice(remainder);
            return Ok(());
        }

        // Check for bracketed paste start
        if let Some(pos) = self.paste_start_finder.find(data) {
            debug!("process_input: bracketed paste started");
            // Process any data before paste start through normal lookback matching
            if pos > 0 {
                self.process_input_lookback(&data[..pos], stdout_fd)?;
            }
            self.in_bracketed_paste = true;
            let paste_data = &data[pos..];
            // Check if paste end is in same chunk
            let search_start = BRACKETED_PASTE_START.len();
            if paste_data.len() > search_start
                && let Some(end_pos) = self.paste_end_finder.find(&paste_data[search_start..])
            {
                let end = search_start + end_pos + BRACKETED_PASTE_END.len();
                write_to_pty_draining(
                    &self.pty_master,
                    &paste_data[..end],
                    &mut self.pty_drain_buffer,
                )?;
                self.in_bracketed_paste = false;
                debug!("process_input: bracketed paste ended (same chunk)");
                if end < paste_data.len() {
                    return self.process_input(&paste_data[end..], stdout_fd);
                }
                return Ok(());
            }
            let (forward, remainder) =
                split_trailing_marker_prefix(paste_data, BRACKETED_PASTE_END);
            if !forward.is_empty() {
                write_to_pty_draining(&self.pty_master, forward, &mut self.pty_drain_buffer)?;
            }
            self.paste_remainder.extend_from_slice(remainder);
            return Ok(());
        }

        self.process_input_lookback(data, stdout_fd)
    }

    /// Byte-by-byte input processing with lookback sequence matching.
    /// Only used for normal (non-paste, non-alt-screen) input.
    fn process_input_lookback<F: AsFd>(&mut self, data: &[u8], stdout_fd: &F) -> Result<()> {
        let lookback_sequence = if self.kitty_mode_enabled() {
            self.config.lookback_sequence_kitty.clone()
        } else {
            self.config.lookback_sequence_legacy.clone()
        };

        for &byte in data {
            if self.in_lookback_mode && byte == 0x03 {
                self.lookback_input_buffer.clear();
                self.exit_lookback_mode(stdout_fd)?;
                continue;
            }

            let lookback_action = self.check_sequence_match(
                byte,
                &mut self.lookback_input_buffer.clone(),
                &lookback_sequence,
            );

            self.lookback_input_buffer.push(byte);

            if self.lookback_input_buffer.len() > lookback_sequence.len() {
                let excess = self.lookback_input_buffer.len() - lookback_sequence.len();
                self.lookback_input_buffer.drain(..excess);
            }

            match lookback_action {
                SequenceMatch::Complete => {
                    self.lookback_input_buffer.clear();
                    if self.in_lookback_mode {
                        self.exit_lookback_mode(stdout_fd)?;
                    } else {
                        self.enter_lookback_mode()?;
                    }
                    continue;
                }
                SequenceMatch::Partial => {
                    // Still might be lookback sequence, don't forward yet
                    continue;
                }
                SequenceMatch::None => {
                    // Not a lookback sequence - forward all buffered bytes
                    if !self.in_lookback_mode {
                        write_all(&self.pty_master, &self.lookback_input_buffer)?;
                    }
                    self.lookback_input_buffer.clear();
                }
            }
        }
        self.lookback_hold_since = (!self.lookback_input_buffer.is_empty()).then(Instant::now);
        Ok(())
    }

    /// Input held back as a possible start of the lookback key is forwarded once no
    /// more bytes follow it, so a lone Esc is not delayed until the next keypress
    fn flush_held_input(&mut self) -> Result<()> {
        let Some(since) = self.lookback_hold_since else {
            return Ok(());
        };
        if since.elapsed() < Duration::from_millis(LOOKBACK_HOLD_MS) {
            return Ok(());
        }
        if !self.in_lookback_mode {
            write_all(&self.pty_master, &self.lookback_input_buffer)?;
        }
        self.lookback_input_buffer.clear();
        self.lookback_hold_since = None;
        Ok(())
    }

    fn check_sequence_match(
        &self,
        byte: u8,
        buffer: &mut Vec<u8>,
        sequence: &[u8],
    ) -> SequenceMatch {
        buffer.push(byte);
        if buffer.len() > sequence.len() {
            let excess = buffer.len() - sequence.len();
            buffer.drain(..excess);
        }
        if buffer.as_slice() == sequence {
            SequenceMatch::Complete
        } else if sequence.starts_with(buffer) {
            SequenceMatch::Partial
        } else {
            SequenceMatch::None
        }
    }

    fn enter_lookback_mode(&mut self) -> Result<()> {
        self.in_lookback_mode = true;
        self.lookback_cache.clear();
        self.vt_render_pending = false;

        let snapshot = self.history_snapshot();
        debug!("enter_lookback_mode: snapshot_bytes={}", snapshot.len());

        // The banner goes above the history: ending on the last screen row keeps the
        // terminal's visible rows aligned with the emulator for the repaint on exit
        let banner = format!(
            "\x1b[7m--- LOOKBACK MODE: press {} or Ctrl+C to exit ---\x1b[0m\r\n",
            self.config.lookback_key
        );
        let stdout_fd = io::stdout();
        write_all(&stdout_fd, CURSOR_HOME)?;
        write_all(&stdout_fd, ERASE_BELOW)?;
        write_all(&stdout_fd, CLEAR_SCROLLBACK)?;
        write_all(&stdout_fd, banner.as_bytes())?;
        write_all(&stdout_fd, &snapshot)?;

        Ok(())
    }

    fn exit_lookback_mode<F: AsFd>(&mut self, stdout_fd: &F) -> Result<()> {
        debug!(
            "exit_lookback_mode: cached_len={}",
            self.lookback_cache.len()
        );
        self.in_lookback_mode = false;

        // Process cached output through VT to update screen state
        let cached = std::mem::take(&mut self.lookback_cache);
        if !cached.is_empty() {
            debug!(
                "exit_lookback_mode: processing {} cached bytes",
                cached.len()
            );
            self.process_output(&cached, stdout_fd)?;
        }

        // Reset sync block state
        self.in_sync_block = false;

        self.forward_winsize()?;

        // Force full render since terminal was showing history; it starts with
        // home + erase below, which also removes the banner when the history is
        // shorter than the screen
        debug!("exit_lookback_mode: rendering VT screen");
        self.force_full_render = true;
        self.render_vt_screen(stdout_fd)?;

        Ok(())
    }

    fn forward_winsize(&mut self) -> Result<()> {
        if let Ok(winsize) = get_terminal_size() {
            debug!(
                "forward_winsize: rows={} cols={}",
                winsize.ws_row, winsize.ws_col
            );
            let (rows, cols) = (nonzero(winsize.ws_row), nonzero(winsize.ws_col));
            self.resize_changed_width |= cols != self.vt_parser.screen().size().1;
            resize_emulator(&mut self.vt_parser, rows, cols);
            self.shadow.screen_mut().set_size(rows, cols);
            // Force full render on next frame since size changed
            self.force_full_render = true;
            // Forward to child process
            unsafe {
                libc::ioctl(
                    self.pty_master.as_raw_fd(),
                    libc::TIOCSWINSZ as libc::c_ulong,
                    &winsize,
                );
            }
        }
        Ok(())
    }

    fn forward_signal(&self, signal: Signal) {
        let pid = Pid::from_raw(self.child.id() as i32);
        let _ = kill(pid, signal);
    }

    fn wait_child(&mut self) -> Result<i32> {
        match self.child.wait() {
            Ok(status) => Ok(exit_code_from_status(status)),
            Err(e) => anyhow::bail!("wait failed: {}", e),
        }
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        // Disable bracketed paste before restoring terminal
        let stdout_fd = io::stdout();
        let _ = write_all(&stdout_fd, BRACKETED_PASTE_DISABLE);

        if let Some(ref termios) = self.original_termios {
            let _ = tcsetattr(io::stdin(), SetArg::TCSANOW, termios);
        }
    }
}

fn nonzero(v: u16) -> NonZeroU16 {
    NonZeroU16::new(v).unwrap_or(NonZeroU16::MIN)
}

/// Resize the emulator so that Claude Code's frame stays on the last rows of the
/// screen, which is where Claude Code repaints it. On a shrink vt100 drops the rows
/// below the cursor before it moves rows into scrollback, and Claude Code rests its
/// cursor above the end of its frame (one row past its last text row). With the cursor
/// moved there first, only rows outside the frame are dropped.
fn resize_emulator(parser: &mut vt100::Parser, rows: NonZeroU16, cols: NonZeroU16) {
    let (old_rows, old_cols) = parser.screen().size();
    if rows < old_rows {
        let text_rows = parser
            .screen()
            .rows(0, old_cols.get())
            .enumerate()
            .filter(|(_, row)| !row.trim().is_empty())
            .map(|(i, _)| i + 1)
            .last()
            .unwrap_or(0);
        let frame_end = (text_rows + 1).min(usize::from(old_rows.get()));
        parser.process(format!("\x1b[{frame_end};1H").as_bytes());
    }
    parser.screen_mut().set_size(rows, cols);
}

fn get_terminal_size() -> Result<Winsize> {
    let mut ws: Winsize = unsafe { std::mem::zeroed() };
    let ret = unsafe {
        libc::ioctl(
            io::stdout().as_raw_fd(),
            libc::TIOCGWINSZ as libc::c_ulong,
            &mut ws,
        )
    };
    if ret == -1 || ws.ws_row == 0 || ws.ws_col == 0 {
        ws.ws_row = 24;
        ws.ws_col = 80;
    }
    Ok(ws)
}

fn exit_code_from_status(status: ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    if let Some(code) = status.code() {
        code
    } else if let Some(signal) = status.signal() {
        128 + signal
    } else {
        1
    }
}

fn setup_raw_mode() -> Result<Option<Termios>> {
    let stdin = io::stdin();
    if !isatty(&stdin).unwrap_or(false) {
        return Ok(None);
    }

    let original = tcgetattr(&stdin).context("tcgetattr failed")?;
    let mut raw = original.clone();
    cfmakeraw(&mut raw);
    tcsetattr(&stdin, SetArg::TCSANOW, &raw).context("tcsetattr failed")?;
    Ok(Some(original))
}

fn setup_signal_handler(signal: Signal, handler: extern "C" fn(libc::c_int)) -> Result<()> {
    let action = SigAction::new(
        SigHandler::Handler(handler),
        SaFlags::SA_RESTART,
        SigSet::empty(),
    );
    unsafe { sigaction(signal, &action) }.context(format!("sigaction {:?} failed", signal))?;
    Ok(())
}

fn setup_signal_handlers() -> Result<()> {
    setup_signal_handler(Signal::SIGWINCH, handle_sigwinch)?;
    setup_signal_handler(Signal::SIGINT, handle_sigint)?;
    setup_signal_handler(Signal::SIGTERM, handle_sigterm)?;
    Ok(())
}

fn set_nonblocking<Fd: AsFd>(fd: &Fd) -> Result<()> {
    let flags = fcntl(fd.as_fd(), FcntlArg::F_GETFL).context("fcntl F_GETFL failed")?;
    let flags = OFlag::from_bits_truncate(flags);
    fcntl(fd.as_fd(), FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))
        .context("fcntl F_SETFL failed")?;
    Ok(())
}

fn write_all<F: AsFd>(fd: &F, data: &[u8]) -> Result<()> {
    let mut written = 0;
    while written < data.len() {
        match write(fd, &data[written..]) {
            Ok(n) => written += n,
            Err(Errno::EAGAIN) | Err(Errno::EINTR) => continue,
            Err(e) => anyhow::bail!("write failed: {}", e),
        }
    }
    Ok(())
}

/// Write data to the pty master, draining child output to stdout when the
/// pty buffer is full. This prevents the classic pty deadlock where both
/// sides block on full buffers (stdin→pty blocks because pty buffer is full,
/// child→pty blocks because its output buffer is also full).
fn write_to_pty_draining<P: AsFd>(pty: &P, data: &[u8], drain_buffer: &mut Vec<u8>) -> Result<()> {
    let mut written = 0;
    let mut read_buf = [0u8; 65536];

    while written < data.len() {
        match write(pty, &data[written..]) {
            Ok(n) => written += n,
            Err(Errno::EAGAIN) => {
                // PTY buffer full — drain child output to make room
                match nix_read(pty, &mut read_buf) {
                    Ok(n) if n > 0 => {
                        drain_buffer.extend_from_slice(&read_buf[..n]);
                    }
                    _ => {
                        // Nothing available yet, poll briefly
                        let mut poll_fds = [PollFd::new(
                            pty.as_fd(),
                            PollFlags::POLLIN | PollFlags::POLLOUT,
                        )];
                        let _ = poll(&mut poll_fds, PollTimeout::from(10u16));
                    }
                }
            }
            Err(Errno::EINTR) => continue,
            Err(e) => anyhow::bail!("write to pty failed: {}", e),
        }
    }
    Ok(())
}

fn nix_read<F: AsFd>(fd: &F, buf: &mut [u8]) -> Result<usize, Errno> {
    read(fd.as_fd(), buf)
}

fn split_trailing_marker_prefix<'a>(data: &'a [u8], marker: &[u8]) -> (&'a [u8], &'a [u8]) {
    let max_prefix = marker.len().saturating_sub(1).min(data.len());
    for prefix_len in (1..=max_prefix).rev() {
        if data.ends_with(&marker[..prefix_len]) {
            let split = data.len() - prefix_len;
            return (&data[..split], &data[split..]);
        }
    }
    (data, &[])
}

#[cfg(test)]
mod tests {
    use super::*;

    // Helper to test Kitty tracking using the real update_kitty_mode_helper
    struct KittyTracker {
        parser: TermwizParser,
        mode_supported: bool,
        mode_stack: u32,
    }

    impl KittyTracker {
        fn new() -> Self {
            Self {
                parser: TermwizParser::new(),
                mode_supported: false,
                mode_stack: 0,
            }
        }

        fn mode_enabled(&self) -> bool {
            self.mode_stack > 0
        }

        fn process_output(&mut self, data: &[u8]) {
            // Uses the real production function
            Proxy::update_kitty_mode_helper(
                &mut self.parser,
                &mut self.mode_stack,
                self.mode_supported,
                data,
            );
        }

        fn process_input(&mut self, data: &[u8]) {
            // Kitty support detection from query response (CSI ? flags u)
            // This is done separately in detect_kitty_support() at startup
            if self.mode_supported {
                return;
            }
            let actions = self.parser.parse_as_vec(data);
            for action in actions {
                if let Action::CSI(CSI::Keyboard(Keyboard::ReportKittyState(_))) = action {
                    self.mode_supported = true;
                    return;
                }
            }
        }
    }

    // Tests for the emulator resize

    /// 12 text rows and the blank row after them on a 10-row screen, cursor 3 rows above
    /// the end of the frame, as Claude Code leaves it
    fn parser_with_frame() -> vt100::Parser {
        let mut parser = vt100::Parser::new(nonzero(10), nonzero(20), 100);
        for i in 0..12 {
            parser.process(format!("row {i}\r\n").as_bytes());
        }
        parser.process(b"\x1b[3A");
        parser
    }

    fn top_row(parser: &vt100::Parser) -> String {
        parser.screen().rows(0, 20).next().unwrap()
    }

    #[test]
    fn test_resize_shrink_keeps_the_end_of_the_frame() {
        let mut parser = parser_with_frame();
        assert_eq!(parser.screen().scrollback_len(), 3);
        resize_emulator(&mut parser, nonzero(6), nonzero(20));
        assert_eq!(parser.screen().scrollback_len(), 7);
        assert_eq!(top_row(&parser), "row 7");
    }

    #[test]
    fn test_resize_grow_takes_rows_back_from_scrollback() {
        let mut parser = parser_with_frame();
        resize_emulator(&mut parser, nonzero(6), nonzero(20));
        resize_emulator(&mut parser, nonzero(12), nonzero(20));
        assert_eq!(parser.screen().scrollback_len(), 1);
        assert_eq!(top_row(&parser), "row 1");
    }

    #[test]
    fn test_resize_shrink_of_a_short_frame_drops_only_blank_rows() {
        let mut parser = vt100::Parser::new(nonzero(10), nonzero(20), 100);
        parser.process(b"row 0\r\nrow 1\r\n");
        resize_emulator(&mut parser, nonzero(4), nonzero(20));
        assert_eq!(parser.screen().scrollback_len(), 0);
        assert_eq!(top_row(&parser), "row 0");
    }

    // Tests for Kitty keyboard protocol tracking

    #[test]
    fn test_kitty_initially_disabled() {
        let tracker = KittyTracker::new();
        assert!(!tracker.mode_enabled());
        assert!(!tracker.mode_supported);
    }

    #[test]
    fn test_kitty_support_detected_from_query_response() {
        let mut tracker = KittyTracker::new();
        // Terminal responds to query with CSI ? flags u
        tracker.process_input(b"\x1b[?1u");
        assert!(tracker.mode_supported);
    }

    #[test]
    fn test_kitty_push_increments_stack() {
        let mut tracker = KittyTracker::new();
        tracker.mode_supported = true;
        // CSI > 1 u = push with flags
        tracker.process_output(b"\x1b[>1u");
        assert_eq!(tracker.mode_stack, 1);
        assert!(tracker.mode_enabled());
    }

    #[test]
    fn test_kitty_push_requires_support() {
        let mut tracker = KittyTracker::new();
        // Push without support detection - should be ignored
        tracker.process_output(b"\x1b[>1u");
        assert_eq!(tracker.mode_stack, 0);
        assert!(!tracker.mode_enabled());
    }

    #[test]
    fn test_kitty_pop_decrements_stack() {
        let mut tracker = KittyTracker::new();
        tracker.mode_supported = true;
        tracker.process_output(b"\x1b[>1u"); // push
        tracker.process_output(b"\x1b[<u"); // pop 1
        assert_eq!(tracker.mode_stack, 0);
        assert!(!tracker.mode_enabled());
    }

    #[test]
    fn test_kitty_pop_with_count() {
        let mut tracker = KittyTracker::new();
        tracker.mode_supported = true;
        tracker.process_output(b"\x1b[>1u"); // push
        tracker.process_output(b"\x1b[>1u"); // push
        tracker.process_output(b"\x1b[>1u"); // push
        assert_eq!(tracker.mode_stack, 3);
        tracker.process_output(b"\x1b[<2u"); // pop 2
        assert_eq!(tracker.mode_stack, 1);
        assert!(tracker.mode_enabled());
    }

    #[test]
    fn test_kitty_pop_saturates_at_zero() {
        let mut tracker = KittyTracker::new();
        tracker.mode_supported = true;
        tracker.process_output(b"\x1b[>1u"); // push
        tracker.process_output(b"\x1b[<5u"); // pop 5 (more than we have)
        assert_eq!(tracker.mode_stack, 0);
        assert!(!tracker.mode_enabled());
    }

    #[test]
    fn test_kitty_split_sequence_across_buffers() {
        let mut tracker = KittyTracker::new();
        tracker.mode_supported = true;
        // Feed the sequence in parts
        tracker.process_output(b"\x1b[>");
        tracker.process_output(b"1u");
        assert_eq!(tracker.mode_stack, 1);
    }

    #[test]
    fn test_kitty_multiple_sequences_in_one_buffer() {
        let mut tracker = KittyTracker::new();
        tracker.mode_supported = true;
        // Push twice, pop once, all in one buffer
        tracker.process_output(b"\x1b[>1u\x1b[>1u\x1b[<u");
        assert_eq!(tracker.mode_stack, 1);
    }

    #[test]
    fn test_kitty_mixed_with_other_sequences() {
        let mut tracker = KittyTracker::new();
        tracker.mode_supported = true;
        // Kitty push mixed with cursor moves and SGR
        tracker.process_output(b"\x1b[H\x1b[>1u\x1b[31m\x1b[2J");
        assert_eq!(tracker.mode_stack, 1);
    }

    #[test]
    fn test_kitty_typical_session_flow() {
        let mut tracker = KittyTracker::new();
        // 1. Terminal responds to query
        tracker.process_input(b"\x1b[?1u");
        assert!(tracker.mode_supported);
        assert!(!tracker.mode_enabled());
        // 2. App pushes keyboard mode
        tracker.process_output(b"\x1b[>1u");
        assert!(tracker.mode_enabled());
        // 3. App pops keyboard mode on exit
        tracker.process_output(b"\x1b[<u");
        assert!(!tracker.mode_enabled());
    }

    // Tests for sequence matching (used for lookback key detection)

    fn check_sequence(buffer: &[u8], byte: u8, sequence: &[u8]) -> SequenceMatch {
        let mut buf = buffer.to_vec();
        buf.push(byte);
        if buf.len() > sequence.len() {
            let excess = buf.len() - sequence.len();
            buf.drain(..excess);
        }
        if buf.as_slice() == sequence {
            SequenceMatch::Complete
        } else if sequence.starts_with(&buf) {
            SequenceMatch::Partial
        } else {
            SequenceMatch::None
        }
    }

    #[test]
    fn test_sequence_match_complete_single_byte() {
        // Single byte sequence (legacy Ctrl+6 = 0x1E)
        let sequence = &[0x1E];
        assert_eq!(check_sequence(&[], 0x1E, sequence), SequenceMatch::Complete);
    }

    #[test]
    fn test_sequence_match_complete_multi_byte() {
        // Multi-byte Kitty sequence: ESC [ 5 4 ; 5 u
        let sequence = b"\x1b[54;5u";
        let mut buffer = Vec::new();
        for &byte in &sequence[..sequence.len() - 1] {
            let result = check_sequence(&buffer, byte, sequence);
            assert_eq!(result, SequenceMatch::Partial);
            buffer.push(byte);
            if buffer.len() > sequence.len() {
                buffer.drain(..buffer.len() - sequence.len());
            }
        }
        // Final byte completes the sequence
        assert_eq!(
            check_sequence(&buffer, sequence[sequence.len() - 1], sequence),
            SequenceMatch::Complete
        );
    }

    #[test]
    fn test_sequence_match_partial() {
        let sequence = b"\x1b[54;5u";
        assert_eq!(check_sequence(&[], 0x1b, sequence), SequenceMatch::Partial);
        assert_eq!(
            check_sequence(&[0x1b], b'[', sequence),
            SequenceMatch::Partial
        );
        assert_eq!(
            check_sequence(&[0x1b, b'['], b'5', sequence),
            SequenceMatch::Partial
        );
    }

    #[test]
    fn test_sequence_match_none_wrong_byte() {
        let sequence = b"\x1b[54;5u";
        // Start with wrong byte
        assert_eq!(check_sequence(&[], b'a', sequence), SequenceMatch::None);
        // Wrong byte after partial match
        assert_eq!(check_sequence(&[0x1b], b'O', sequence), SequenceMatch::None);
    }

    #[test]
    fn test_sequence_match_buffer_rolling() {
        // Test that the rolling buffer properly handles the case where
        // random bytes precede the actual sequence. The buffer keeps
        // only the last N bytes where N = sequence.len()
        let sequence = b"\x1b[54;5u"; // 7 bytes
        // User types random chars - no match
        assert_eq!(check_sequence(&[], b'a', sequence), SequenceMatch::None);
        assert_eq!(check_sequence(b"a", b'b', sequence), SequenceMatch::None);
        // Buffer [a, b, ESC] doesn't start sequence (sequence starts with ESC)
        assert_eq!(check_sequence(b"ab", 0x1b, sequence), SequenceMatch::None);
        // After more typing, old bytes get trimmed from buffer
        // When buffer finally contains just ESC at the right position, it matches
        // But with rolling buffer, we need the EXACT prefix
        // Fresh start: ESC alone is a partial match
        assert_eq!(check_sequence(&[], 0x1b, sequence), SequenceMatch::Partial);
    }

    #[test]
    fn test_sequence_match_interleaved_typing() {
        // User types "ab" then the lookback sequence
        let sequence = &[0x1E];
        assert_eq!(check_sequence(&[], b'a', sequence), SequenceMatch::None);
        assert_eq!(check_sequence(b"a", b'b', sequence), SequenceMatch::None);
        assert_eq!(
            check_sequence(b"ab", 0x1E, sequence),
            SequenceMatch::Complete
        );
    }

    #[test]
    fn test_split_trailing_marker_prefix_no_match() {
        let marker = b"\x1b[201~";
        let data = b"hello world";
        let (forward, remainder) = split_trailing_marker_prefix(data, marker);
        assert_eq!(forward, b"hello world");
        assert!(remainder.is_empty());
    }

    #[test]
    fn test_split_trailing_marker_prefix_one_byte() {
        let marker = b"\x1b[201~";
        let data = b"paste data\x1b";
        let (forward, remainder) = split_trailing_marker_prefix(data, marker);
        assert_eq!(forward, b"paste data");
        assert_eq!(remainder, b"\x1b");
    }

    #[test]
    fn test_split_trailing_marker_prefix_partial() {
        let marker = b"\x1b[201~";
        let data = b"paste data\x1b[20";
        let (forward, remainder) = split_trailing_marker_prefix(data, marker);
        assert_eq!(forward, b"paste data");
        assert_eq!(remainder, b"\x1b[20");
    }

    #[test]
    fn test_split_trailing_marker_prefix_max_prefix() {
        let marker = b"\x1b[201~";
        let data = b"paste\x1b[201";
        let (forward, remainder) = split_trailing_marker_prefix(data, marker);
        assert_eq!(forward, b"paste");
        assert_eq!(remainder, b"\x1b[201");
    }

    #[test]
    fn test_split_trailing_marker_prefix_full_marker_not_held() {
        let marker = b"\x1b[201~";
        let data = b"paste\x1b[201~";
        let (forward, remainder) = split_trailing_marker_prefix(data, marker);
        assert_eq!(forward, b"paste\x1b[201~");
        assert!(remainder.is_empty());
    }

    #[test]
    fn test_split_trailing_marker_prefix_false_positive() {
        let marker = b"\x1b[201~";
        let data = b"paste data\x1b[999";
        let (forward, remainder) = split_trailing_marker_prefix(data, marker);
        assert_eq!(forward, b"paste data\x1b[999");
        assert!(remainder.is_empty());
    }

    #[test]
    fn test_split_trailing_marker_prefix_empty_data() {
        let marker = b"\x1b[201~";
        let data = b"";
        let (forward, remainder) = split_trailing_marker_prefix(data, marker);
        assert!(forward.is_empty());
        assert!(remainder.is_empty());
    }

    #[test]
    fn test_split_trailing_marker_prefix_data_shorter_than_marker() {
        let marker = b"\x1b[201~";
        let data = b"\x1b[2";
        let (forward, remainder) = split_trailing_marker_prefix(data, marker);
        assert!(forward.is_empty());
        assert_eq!(remainder, b"\x1b[2");
    }
}
