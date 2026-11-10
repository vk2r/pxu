//! Página Storage (`d`): estado + reducer, sin IO (el bucle lee los datos en un hilo y se los entrega con `set_data`).
use crossterm::event::{KeyCode, KeyEvent};
use pxu::share::storage::{SortBy, StorageData};
use std::cell::Cell;

/// Qué pide una tecla al bucle / a la app.
#[derive(Debug, PartialEq, Eq)]
pub enum StorageKey {
    None,
    /// Volver a la lista principal.
    Back,
    /// `r`: volver a leer (sin bloquear: se conserva lo ya mostrado).
    Refresh,
    Help,
}

pub struct StorageState {
    pub data: Option<StorageData>,
    /// Hay una lectura en vuelo.
    pub loading: bool,
    pub sel: usize,
    pub sort: SortBy,
    /// `f`: solo los guests con alguna alerta.
    pub only_alerts: bool,
    /// Enter: detalle de espacio de este guest.
    pub detail: Option<u32>,
    pub top: usize,
    /// Lo fija el dibujo: primera fila visible, filas que caben, alto del detalle.
    pub off: Cell<usize>,
    pub rows_h: Cell<usize>,
    pub body_h: Cell<usize>,
    pub detail_len: Cell<usize>,
}

impl Default for StorageState {
    fn default() -> Self {
        StorageState { data: None, loading: false, sel: 0, sort: SortBy::Used, only_alerts: false, detail: None, top: 0, off: Cell::new(0), rows_h: Cell::new(0), body_h: Cell::new(0), detail_len: Cell::new(0) }
    }
}

impl StorageState {
    /// Vmids en el orden y con el filtro actuales.
    pub fn ids(&self) -> Vec<u32> {
        self.data.as_ref().map_or(vec![], |d| d.view(self.sort, self.only_alerts).iter().map(|g| g.vmid).collect())
    }

    pub fn selected(&self) -> Option<u32> {
        self.ids().get(self.sel).copied()
    }

    /// Entrega datos nuevos conservando el guest seleccionado (si sigue ahí).
    pub fn set_data(&mut self, d: StorageData) {
        let keep = self.selected();
        self.loading = d.partial;
        self.data = Some(d);
        self.reselect(keep);
        if self.detail.is_some_and(|v| self.data.as_ref().is_some_and(|d| !d.guests.iter().any(|g| g.vmid == v))) {
            self.detail = None;
        }
    }

    fn reselect(&mut self, keep: Option<u32>) {
        let ids = self.ids();
        self.sel = keep.and_then(|v| ids.iter().position(|i| *i == v)).unwrap_or(0).min(ids.len().saturating_sub(1));
    }

    fn move_by(&mut self, d: isize) {
        let n = self.ids().len();
        self.sel = (self.sel as isize + d).clamp(0, n.saturating_sub(1) as isize) as usize;
    }

    fn scroll(&mut self, d: isize) {
        let max = self.detail_len.get().saturating_sub(self.body_h.get().max(1));
        self.top = (self.top as isize + d).clamp(0, max as isize) as usize;
    }

    pub fn on_key(&mut self, k: KeyEvent) -> StorageKey {
        let page = self.rows_h.get().saturating_sub(1).max(1) as isize;
        if self.detail.is_some() {
            match k.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Backspace | KeyCode::Enter => {
                    self.detail = None;
                    self.top = 0;
                }
                KeyCode::Up | KeyCode::Char('k') => self.scroll(-1),
                KeyCode::Down | KeyCode::Char('j') => self.scroll(1),
                KeyCode::PageUp => self.scroll(-(self.body_h.get() as isize - 1).max(1)),
                KeyCode::PageDown => self.scroll((self.body_h.get() as isize - 1).max(1)),
                KeyCode::Char('g') | KeyCode::Home => self.top = 0,
                KeyCode::Char('G') | KeyCode::End => self.scroll(isize::MAX / 2),
                KeyCode::Char('r') => return StorageKey::Refresh,
                KeyCode::Char('?') => return StorageKey::Help,
                _ => {}
            }
            return StorageKey::None;
        }
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Backspace => return StorageKey::Back,
            KeyCode::Up | KeyCode::Char('k') => self.move_by(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_by(1),
            KeyCode::PageUp => self.move_by(-page),
            KeyCode::PageDown => self.move_by(page),
            KeyCode::Home | KeyCode::Char('g') => self.sel = 0,
            KeyCode::End | KeyCode::Char('G') => self.move_by(isize::MAX / 2),
            KeyCode::Enter | KeyCode::Char('d') => {
                if let Some(v) = self.selected() {
                    self.detail = Some(v);
                    self.top = 0;
                }
            }
            KeyCode::Char('S') => {
                let keep = self.selected();
                self.sort = self.sort.next();
                self.reselect(keep);
            }
            KeyCode::Char('f') => {
                let keep = self.selected();
                self.only_alerts = !self.only_alerts;
                self.reselect(keep);
            }
            KeyCode::Char('r') => return StorageKey::Refresh,
            KeyCode::Char('?') => return StorageKey::Help,
            _ => {}
        }
        StorageKey::None
    }
}
