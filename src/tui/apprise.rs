//! Diálogo de `[apprise]` (`n` en Config): estado + reducer, sin IO (como `SchedModal`).
//! El secreto guardado nunca se muestra: el campo está vacío ("conservar") y lo escrito se pinta como puntos.
use crossterm::event::{KeyCode, KeyEvent};
use pxu::share::config::{tag_char_ok, AppriseEdit, AppriseMode, APPRISE_EVENTS};
use pxu::share::notify::AppriseCfg;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Url,
    Mode,
    Secret,
    Tag,
    Event(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Edit,
    Confirm,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AKey {
    None,
    Close,
    /// Validar la config resultante antes de confirmar.
    Check(AppriseEdit),
    Save(AppriseEdit),
}

pub struct AppriseModal {
    pub api_url: String,
    pub mode: AppriseMode,
    pub secret: String,
    pub tag: String,
    pub on: [bool; 4],
    pub field: usize,
    pub stage: Stage,
    pub error: Option<String>,
    pub lines: Vec<String>,
    has_key: bool,
    has_urls: bool,
    submitted: Option<AppriseEdit>,
}

impl AppriseModal {
    pub fn new(c: &AppriseCfg) -> AppriseModal {
        let mut on = [false; 4];
        for (i, e) in APPRISE_EVENTS.iter().enumerate() {
            on[i] = c.on.iter().any(|x| x == e);
        }
        let mode = if !c.has_key() && c.has_urls() { AppriseMode::Urls } else { AppriseMode::Key };
        AppriseModal { api_url: c.api_url.clone(), mode, secret: String::new(), tag: c.tag.clone(), on, field: 0, stage: Stage::Edit, error: None, lines: Vec::new(), has_key: c.has_key(), has_urls: c.has_urls(), submitted: None }
    }

    pub fn fields(&self) -> Vec<Field> {
        let mut v = vec![Field::Url, Field::Mode, Field::Secret, Field::Tag];
        v.extend((0..APPRISE_EVENTS.len()).map(Field::Event));
        v
    }

    /// ¿Hay un secreto guardado del modo elegido (que se conserva con el campo vacío)?
    pub fn stored(&self) -> bool {
        match self.mode {
            AppriseMode::Key => self.has_key,
            AppriseMode::Urls => self.has_urls,
        }
    }

    fn edit(&self) -> AppriseEdit {
        let on = APPRISE_EVENTS.iter().enumerate().filter(|(i, _)| self.on[*i]).map(|(_, e)| e.to_string()).collect();
        AppriseEdit { api_url: self.api_url.trim().to_string(), mode: self.mode, secret: self.secret.clone(), on, tag: self.tag.trim().to_string() }
    }

    /// Respuesta de `plan_apprise`: pasa a la confirmación (secretos enmascarados) o muestra por qué no.
    pub fn checked(&mut self, r: Result<Vec<String>, String>) {
        match r {
            Err(e) => self.error = Some(e),
            Ok(lines) => {
                self.lines = lines;
                self.submitted = Some(self.edit());
                self.stage = Stage::Confirm;
            }
        }
    }

    pub fn on_key(&mut self, k: KeyEvent) -> AKey {
        if self.stage == Stage::Confirm {
            return match k.code {
                KeyCode::Char('y') | KeyCode::Enter => self.submitted.take().map_or(AKey::Close, AKey::Save),
                KeyCode::Esc | KeyCode::Char('n' | 'q') => AKey::Close,
                KeyCode::Backspace | KeyCode::Char('b') => {
                    self.stage = Stage::Edit;
                    self.submitted = None;
                    AKey::None
                }
                _ => AKey::None,
            };
        }
        self.error = None;
        let n = self.fields().len();
        let cur = self.fields()[self.field.min(n - 1)];
        match k.code {
            KeyCode::Esc => return AKey::Close,
            KeyCode::Tab | KeyCode::Down => self.field = (self.field + 1) % n,
            KeyCode::BackTab | KeyCode::Up => self.field = (self.field + n - 1) % n,
            KeyCode::Enter => return AKey::Check(self.edit()),
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') if matches!(cur, Field::Mode | Field::Event(_)) => match cur {
                Field::Mode => {
                    self.mode = if self.mode == AppriseMode::Key { AppriseMode::Urls } else { AppriseMode::Key };
                    self.secret.clear();
                }
                Field::Event(i) => self.on[i] = !self.on[i],
                _ => {}
            },
            KeyCode::Backspace => match cur {
                Field::Url => drop(self.api_url.pop()),
                Field::Secret => drop(self.secret.pop()),
                Field::Tag => drop(self.tag.pop()),
                _ => {}
            },
            KeyCode::Char(c) if !c.is_control() => match cur {
                Field::Url if c != ' ' && self.api_url.len() < 200 => self.api_url.push(c),
                // URLs: separadas por coma; los espacios se ignoran al teclear (se pegan bien)
                Field::Tag if tag_char_ok(c) && self.tag.len() < 200 => self.tag.push(c),
                Field::Secret if c != ' ' && self.secret.len() < 1000 => self.secret.push(c),
                _ => {}
            },
            _ => {}
        }
        AKey::None
    }
}
