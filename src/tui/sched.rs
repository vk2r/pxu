//! Diálogo del horario del update automático (`S`): estado + reducer, sin IO (como `AutoModal`).
use crossterm::event::{KeyCode, KeyEvent};
use pxu::share::schedule::{self, CalendarCheck, SchedulePlan, TimerStatus};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    Daily,
    Weekdays,
    Weekly,
    Custom,
}

impl Preset {
    pub fn label(self) -> &'static str {
        match self {
            Preset::Daily => "daily",
            Preset::Weekdays => "Mon-Fri",
            Preset::Weekly => "weekly",
            Preset::Custom => "custom OnCalendar",
        }
    }
    fn step(self, d: i32) -> Preset {
        const ALL: [Preset; 4] = [Preset::Daily, Preset::Weekdays, Preset::Weekly, Preset::Custom];
        let i = ALL.iter().position(|p| *p == self).unwrap_or(0) as i32;
        ALL[(i + d).rem_euclid(4) as usize]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Enable,
    Preset,
    Day,
    Time,
    Custom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Edit,
    Confirm,
}

/// Resultado de una tecla en el diálogo.
#[derive(Debug, PartialEq, Eq)]
pub enum SchedKey {
    None,
    Close,
    /// Validar este `OnCalendar` con systemd antes de confirmar.
    Check(String),
    Save(SchedulePlan),
}

pub struct SchedModal {
    pub status: TimerStatus,
    pub enable: bool,
    pub preset: Preset,
    pub day: usize,
    pub time: String,
    pub custom: String,
    pub field: usize,
    pub stage: Stage,
    pub error: Option<String>,
    /// Avisos de contexto (config ausente, sin CT con auto-update…).
    pub warnings: Vec<String>,
    /// Texto de la confirmación («hoy: … → nuevo»).
    pub lines: Vec<String>,
    pending: Option<SchedulePlan>,
    expr: Option<String>,
}

/// Interpreta un `OnCalendar` efectivo como preset (acepta `HH:MM` y `HH:MM:SS` con segundos 00).
fn as_preset(cal: &str) -> Option<(Preset, usize, String)> {
    let cal = cal.trim();
    let (days, rest) = match cal.split_once(" *-*-* ") {
        Some((d, r)) => (Some(d), r),
        None => (None, cal.strip_prefix("*-*-* ")?),
    };
    let t = rest.strip_suffix(":00").filter(|t| t.len() == 5).unwrap_or(rest);
    schedule::parse_time(t).ok()?;
    let t = t.to_string();
    match days {
        None => Some((Preset::Daily, 0, t)),
        Some("Mon..Fri") => Some((Preset::Weekdays, 0, t)),
        Some(d) => schedule::DAYS.iter().position(|x| *x == d).map(|i| (Preset::Weekly, i, t)),
    }
}

impl SchedModal {
    pub fn new(status: TimerStatus, warnings: Vec<String>) -> SchedModal {
        let (mut preset, mut day, mut time, mut custom) = (Preset::Daily, 0, "04:30".to_string(), String::new());
        if let Some(cal) = status.calendars.first() {
            match as_preset(cal) {
                Some((p, d, t)) => (preset, day, time) = (p, d, t),
                None => (preset, custom) = (Preset::Custom, cal.clone()),
            }
        }
        SchedModal { status, enable: true, preset, day, time, custom, field: 0, stage: Stage::Edit, error: None, warnings, lines: Vec::new(), pending: None, expr: None }
    }

    pub fn fields(&self) -> Vec<Field> {
        let mut v = vec![Field::Enable, Field::Preset];
        if !self.enable {
            return v;
        }
        match self.preset {
            Preset::Weekly => v.extend([Field::Day, Field::Time]),
            Preset::Custom => v.push(Field::Custom),
            _ => v.push(Field::Time),
        }
        v
    }

    fn current(&self) -> Field {
        let f = self.fields();
        f[self.field.min(f.len() - 1)]
    }

    /// `OnCalendar` que sale de los campos.
    pub fn expr(&self) -> Result<String, String> {
        if self.preset == Preset::Custom {
            return schedule::check_expr(&self.custom).map(str::to_string);
        }
        let (h, m) = schedule::parse_time(&self.time)?;
        Ok(match self.preset {
            Preset::Daily => schedule::daily(h, m),
            Preset::Weekdays => schedule::weekdays(h, m),
            _ => schedule::weekly(self.day, h, m),
        })
    }

    /// Estado de hoy, en una línea.
    pub fn today(&self) -> String {
        let s = &self.status;
        if !s.installed {
            return "not installed".into();
        }
        let cal = s.calendars.first().map_or("no schedule".to_string(), |c| c.clone());
        format!("{}, {cal}", if s.enabled { "enabled" } else { "disabled" })
    }

    pub fn on_key(&mut self, k: KeyEvent) -> SchedKey {
        if self.stage == Stage::Confirm {
            return match k.code {
                KeyCode::Char('y') | KeyCode::Enter => match self.pending.take() {
                    Some(p) => SchedKey::Save(p),
                    None => SchedKey::Close,
                },
                KeyCode::Esc | KeyCode::Char('n' | 'q') => SchedKey::Close,
                KeyCode::Backspace | KeyCode::Char('b') => {
                    self.stage = Stage::Edit;
                    self.pending = None;
                    SchedKey::None
                }
                _ => SchedKey::None,
            };
        }
        self.error = None;
        let n = self.fields().len();
        let cur = self.current();
        match k.code {
            KeyCode::Esc => return SchedKey::Close,
            KeyCode::Tab | KeyCode::Down => self.field = (self.field + 1) % n,
            KeyCode::BackTab | KeyCode::Up => self.field = (self.field + n - 1) % n,
            KeyCode::Enter => return self.submit(),
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') if matches!(cur, Field::Enable | Field::Preset | Field::Day) => {
                let d = if k.code == KeyCode::Left { -1 } else { 1 };
                match cur {
                    Field::Enable => self.enable = !self.enable,
                    Field::Preset => self.preset = self.preset.step(d),
                    _ => self.day = (self.day as i32 + d).rem_euclid(7) as usize,
                }
                self.field = self.field.min(self.fields().len() - 1);
            }
            KeyCode::Backspace => match cur {
                Field::Time => drop(self.time.pop()),
                Field::Custom => drop(self.custom.pop()),
                _ => {}
            },
            KeyCode::Char(c) => match cur {
                Field::Time if (c.is_ascii_digit() || c == ':') && self.time.len() < 5 => self.time.push(c),
                Field::Custom if !c.is_control() && self.custom.len() < 120 => self.custom.push(c),
                _ => {}
            },
            _ => {}
        }
        SchedKey::None
    }

    fn submit(&mut self) -> SchedKey {
        if !self.enable {
            if !self.status.installed {
                self.error = Some("the timer is not installed: nothing to disable".into());
                return SchedKey::None;
            }
            self.pending = Some(SchedulePlan { install_units: false, on_calendar: None, enable: false });
            self.lines = vec![format!("today: {} → disabled", self.today()), "the timer stops (systemctl disable --now); the schedule file is kept".into()];
            self.stage = Stage::Confirm;
            return SchedKey::None;
        }
        match self.expr() {
            Ok(e) => {
                self.expr = Some(e.clone());
                SchedKey::Check(e)
            }
            Err(e) => {
                self.error = Some(e);
                SchedKey::None
            }
        }
    }

    /// Respuesta de `validate_calendar`: pasa a la confirmación (con la próxima ejecución) o muestra el error.
    pub fn checked(&mut self, r: Result<CalendarCheck, String>) {
        let Some(expr) = self.expr.take() else { return };
        match r {
            Err(e) => self.error = Some(e),
            Ok(c) => {
                let install = !self.status.installed;
                self.lines = vec![format!("today: {} → enabled, {expr}", self.today())];
                self.lines.push(format!("next run: {} (server time)", c.next.as_deref().unwrap_or("unknown")));
                if install {
                    self.lines.push(format!("the units are not installed: pxu-update.service and .timer will be written in {}", schedule::UNIT_DIR));
                }
                self.lines.push(format!("saved as a drop-in: {}", schedule::DROPIN_PATH));
                self.pending = Some(SchedulePlan { install_units: install, on_calendar: Some(expr), enable: true });
                self.stage = Stage::Confirm;
            }
        }
    }
}
