//! Página Logs (`l`): lista de `run-*.log` y visor. Estado + reducer, sin IO (el bucle lee los archivos).
use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use pxu::share::update::runlog::LogEntry;
use std::cell::Cell;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const PAGE: usize = 10;
const DOUBLE_CLICK: Duration = Duration::from_millis(500);

/// Qué pide una tecla o un clic al bucle.
#[derive(Debug, PartialEq, Eq)]
pub enum LogsKey {
    None,
    Quit,
    /// Salir de la página (a la lista principal).
    Back,
    /// Leer y abrir este log en el visor.
    Open(PathBuf),
}

pub struct Viewer {
    pub title: String,
    pub lines: Vec<String>,
    pub top: usize,
}

#[derive(Default)]
pub struct LogsState {
    pub entries: Vec<LogEntry>,
    pub sel: usize,
    pub viewer: Option<Viewer>,
    /// Lo fija el dibujo: primera fila visible de la lista, filas que caben, fila de pantalla de la primera fila.
    pub off: Cell<usize>,
    pub rows_h: Cell<usize>,
    pub rows_y: Cell<u16>,
    pub body_h: Cell<usize>,
    last_click: Option<(usize, Instant)>,
}

impl LogsState {
    pub fn new(entries: Vec<LogEntry>) -> LogsState {
        LogsState { entries, ..Default::default() }
    }

    fn max_top(&self) -> usize {
        self.viewer.as_ref().map_or(0, |v| v.lines.len().saturating_sub(self.body_h.get().max(1)))
    }

    fn scroll_to(&mut self, top: usize) {
        let max = self.max_top();
        if let Some(v) = self.viewer.as_mut() {
            v.top = top.min(max);
        }
    }

    fn scroll_by(&mut self, d: isize) {
        let cur = self.viewer.as_ref().map_or(0, |v| v.top);
        self.scroll_to((cur as isize + d).max(0) as usize);
    }

    fn open_sel(&self) -> LogsKey {
        self.entries.get(self.sel).map_or(LogsKey::None, |e| LogsKey::Open(e.path.clone()))
    }

    /// Cambia al log siguiente (`n`, más viejo) o anterior (`p`, más nuevo) de la lista.
    fn step(&mut self, d: isize) -> LogsKey {
        let n = self.entries.len() as isize;
        let to = self.sel as isize + d;
        if to < 0 || to >= n {
            return LogsKey::None;
        }
        self.sel = to as usize;
        self.open_sel()
    }

    pub fn on_key(&mut self, k: KeyEvent) -> LogsKey {
        if self.viewer.is_some() {
            return self.viewer_key(k);
        }
        let n = self.entries.len();
        match k.code {
            KeyCode::Char('q') => return LogsKey::Quit,
            KeyCode::Esc | KeyCode::Backspace | KeyCode::Char('l') => return LogsKey::Back,
            KeyCode::Down | KeyCode::Char('j') => self.sel = (self.sel + 1).min(n.saturating_sub(1)),
            KeyCode::Up | KeyCode::Char('k') => self.sel = self.sel.saturating_sub(1),
            KeyCode::PageDown => self.sel = (self.sel + PAGE).min(n.saturating_sub(1)),
            KeyCode::PageUp => self.sel = self.sel.saturating_sub(PAGE),
            KeyCode::Home | KeyCode::Char('g') => self.sel = 0,
            KeyCode::End | KeyCode::Char('G') => self.sel = n.saturating_sub(1),
            KeyCode::Enter => return self.open_sel(),
            _ => {}
        }
        LogsKey::None
    }

    fn viewer_key(&mut self, k: KeyEvent) -> LogsKey {
        let page = self.body_h.get().saturating_sub(1).max(1) as isize;
        match k.code {
            KeyCode::Char('q') | KeyCode::Esc | KeyCode::Backspace => self.viewer = None,
            KeyCode::Down | KeyCode::Char('j') => self.scroll_by(1),
            KeyCode::Up | KeyCode::Char('k') => self.scroll_by(-1),
            KeyCode::PageDown | KeyCode::Char(' ') => self.scroll_by(page),
            KeyCode::PageUp => self.scroll_by(-page),
            KeyCode::Home | KeyCode::Char('g') => self.scroll_to(0),
            KeyCode::End | KeyCode::Char('G') => self.scroll_to(usize::MAX),
            KeyCode::Char('n') => return self.step(1),
            KeyCode::Char('p') => return self.step(-1),
            _ => {}
        }
        LogsKey::None
    }

    pub fn on_mouse(&mut self, m: MouseEvent, now: Instant) -> LogsKey {
        match m.kind {
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                let d: isize = if m.kind == MouseEventKind::ScrollDown { 3 } else { -3 };
                if self.viewer.is_some() {
                    self.scroll_by(d);
                } else {
                    let n = self.entries.len();
                    self.sel = (self.sel as isize + d).clamp(0, n.saturating_sub(1) as isize) as usize;
                }
                LogsKey::None
            }
            MouseEventKind::Down(MouseButton::Left) if self.viewer.is_none() => {
                let (y0, h) = (self.rows_y.get(), self.rows_h.get());
                if m.row < y0 || (m.row - y0) as usize >= h {
                    return LogsKey::None;
                }
                let i = self.off.get() + (m.row - y0) as usize;
                if i >= self.entries.len() {
                    return LogsKey::None;
                }
                self.sel = i;
                let double = self.last_click.is_some_and(|(j, t)| j == i && now.duration_since(t) <= DOUBLE_CLICK);
                self.last_click = if double { None } else { Some((i, now)) };
                if double { self.open_sel() } else { LogsKey::None }
            }
            _ => LogsKey::None,
        }
    }
}
