//! Vistas: lista principal (DESIGN §5.1) y detalle LXC/VM (§5.2). Solo dibujan; no tocan IO.
use super::app::{App, Filter, QItem, QState, View};
use pxu::share::labels::{index_label, status_label, summary_line};
use pxu::share::update::check::Status;
use pxu::share::fmt::{self, trunc};
use pxu::share::model::{Guest, GuestKind, RunState};
use pxu::share::proxmox::conf::{parse_props, parse_size, prop};
use pxu::share::proxmox::hostcheck::{self, HostInfo};
use pxu::share::update::Outcome;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

pub const MIN_W: u16 = 80;
pub const MIN_H: u16 = 24;

/// Paleta de 16 colores (DESIGN §3). Con `NO_COLOR` solo quedan bold/reverse/dim textual.
#[derive(Clone, Copy)]
pub struct Theme {
    pub color: bool,
}

impl Theme {
    pub fn from_env() -> Theme {
        Theme { color: std::env::var_os("NO_COLOR").map_or(true, |v| v.is_empty()) }
    }
    fn fg(&self, c: Color) -> Style {
        if self.color {
            Style::default().fg(c)
        } else {
            Style::default()
        }
    }
    fn accent(&self) -> Style {
        self.fg(Color::Cyan)
    }
    fn ok(&self) -> Style {
        self.fg(Color::Green)
    }
    fn warn(&self) -> Style {
        self.fg(Color::Yellow)
    }
    fn vm(&self) -> Style {
        self.fg(Color::Magenta)
    }
    fn muted(&self) -> Style {
        self.fg(Color::DarkGray)
    }
    fn bold(&self) -> Style {
        Style::default().add_modifier(Modifier::BOLD)
    }
    fn sel(&self) -> Style {
        Style::default().add_modifier(Modifier::REVERSED)
    }
}

/// Barra de atajos `[tecla] acción  [tecla] acción`. Si no cabe en `w`, quita acciones enteras desde el
/// final, pero conserva siempre las últimas `keep` (p. ej. `[?] help` y `[q] quit`).
fn keybar(th: &Theme, items: &[(&str, &str)], keep: usize, w: usize) -> Line<'static> {
    let len = |it: &(&str, &str)| it.0.chars().count() + it.1.chars().count() + 3; // "[k] acción"
    let mut v: Vec<(&str, &str)> = items.to_vec();
    let total = |v: &[(&str, &str)]| v.iter().map(len).sum::<usize>() + 2 * v.len().saturating_sub(1);
    while total(&v) > w && v.len() > keep {
        v.remove(v.len() - keep - 1);
    }
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (i, (k, a)) in v.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled("[", th.muted()));
        spans.push(Span::styled(k.to_string(), th.accent()));
        spans.push(Span::styled("]", th.muted()));
        spans.push(Span::raw(format!(" {a}")));
    }
    Line::from(spans)
}

/// Igual que `keybar`, con la sangría de un espacio que llevan las líneas de los modales.
fn mbar(th: &Theme, items: &[(&str, &str)], keep: usize, w: usize) -> Line<'static> {
    let mut l = keybar(th, items, keep, w.saturating_sub(1));
    l.spans.insert(0, Span::raw(" "));
    l
}

/// Barras de atajos en una sola secuencia, con huecos de 2 espacios y alineadas a la izquierda: van en una
/// línea si caben en `w`; si no, pasan a la siguiente llenando cada línea con todas las acciones que
/// entren. Si hacen falta más de `max_lines`, quita acciones antes de las últimas `keep`.
fn keybar_wrap(th: &Theme, items: &[(&str, &str)], keep: usize, w: usize, max_lines: usize) -> Vec<Line<'static>> {
    let len = |it: &(&str, &str)| it.0.chars().count() + it.1.chars().count() + 3;
    let mut v: Vec<(&str, &str)> = items.to_vec();
    loop {
        let mut rows: Vec<Vec<(&str, &str)>> = vec![Vec::new()];
        let mut used = 0;
        for it in &v {
            let row = rows.last_mut().unwrap();
            if !row.is_empty() && used + 2 + len(it) > w {
                rows.push(vec![*it]);
                used = len(it);
            } else {
                used += len(it) + if row.is_empty() { 0 } else { 2 };
                row.push(*it);
            }
        }
        if rows.len() <= max_lines || v.len() <= keep {
            return rows.iter().map(|r| keybar(th, r, 0, usize::MAX)).collect();
        }
        v.remove(v.len() - keep - 1);
    }
}

pub fn draw(f: &mut Frame, app: &App, th: &Theme) {
    let area = f.area();
    if area.width < MIN_W || area.height < MIN_H {
        f.render_widget(Paragraph::new("terminal too small (min 80×24)"), area);
        return;
    }
    match app.view {
        View::List => draw_list(f, app, th, area),
        View::Detail(id) => draw_detail(f, app, th, area, id),
        View::Check => draw_check(f, app, th, area),
        View::Update => draw_update(f, app, th, area),
        View::Config => draw_config(f, app, th, area),
        View::Logs => draw_logs(f, app, th, area),
        View::Storage => draw_storage(f, app, th, area),
    }
    if app.modal.is_some() {
        draw_modal(f, app, th, area);
    }
    if app.col_modal.is_some() {
        draw_col_modal(f, app, th, area);
    }
    if app.host_modal.is_some() {
        draw_host_modal(f, app, th, area);
    }
    if app.auto_modal.is_some() {
        draw_auto_modal(f, app, th, area);
    }
    if app.backup_modal.is_some() {
        draw_backup_modal(f, app, th, area);
    }
    if app.backup_del.is_some() {
        draw_backup_del_modal(f, app, th, area);
    }
    if app.sched.is_some() {
        draw_sched_modal(f, app, th, area);
    }
    if app.apprise_modal.is_some() {
        draw_apprise_modal(f, app, th, area);
    }
    if app.log_modal.is_some() {
        draw_log_modal(f, app, th, area);
    }
    if app.update.confirm_cancel {
        draw_cancel(f, app, th, area);
    }
    if app.help {
        draw_help(f, app, th, area);
    }
}

fn badges(app: &App) -> String {
    let mut s = String::new();
    if app.read_only {
        s.push_str(" [RO]");
    }
    if app.mock && !app.hide_mock_badge {
        s.push_str(" [MOCK]");
    }
    s
}

fn two_sided(left: Vec<Span<'static>>, right: String, w: u16) -> Line<'static> {
    let lw: usize = left.iter().map(|s| s.content.chars().count()).sum();
    let rw = right.chars().count();
    let gap = (w as usize).saturating_sub(lw + rw).max(1);
    let mut v = left;
    v.push(Span::raw(" ".repeat(gap)));
    v.push(Span::raw(right));
    Line::from(v)
}

// ---------------------------------------------------------------- lista

struct Col {
    title: &'static str,
    w: usize,
}

/// `cw(título, ancho_gpu)`: contenido más largo de esa columna entre los guests actuales (sin contar la cabecera).
/// `shown`: columnas opcionales visibles (`[ui] columns`). `gpu`: algún guest usa GPU.
/// ≥112 columnas: la columna GPU va antes de UPD; por debajo sustituye a OS. Sin GPU en uso no aparece.
/// Cada columna mide max(cabecera, contenido); CPU es fija (su valor cambia cada segundo y la tabla no debe saltar).
/// NAME va primero y solo se recorta con … si no cabe. TAGS recibe lo que sobra, hasta su texto más largo;
/// si le quedan menos de 6 columnas se oculta (cabecera incluida). Lo que sobra queda vacío.
fn columns(width: u16, gpu: bool, shown: &[String], cw: &dyn Fn(&str, usize) -> usize) -> Vec<Col> {
    let on = |k: &str| shown.iter().any(|c| c == k);
    let c = |title: &'static str, fixed: Option<usize>, gw: usize| Col { title, w: fixed.unwrap_or_else(|| cw(title, gw).max(title.chars().count())) };
    if width >= 112 {
        let mut pre = vec![c("ID", None, 8)];
        if on("type") {
            pre.push(c("TYPE", None, 8));
        }
        let mut post: Vec<Col> = vec![c("STATE", None, 8)];
        for (k, t) in [("os", "OS"), ("cpu", "CPU"), ("ram", "RAM"), ("disk", "DISK")] {
            if on(k) {
                post.push(c(t, if t == "CPU" { Some(6) } else { None }, 8));
            }
        }
        if gpu && on("gpu") {
            post.push(c("GPU", None, 8));
        }
        if on("upd") {
            post.push(c("UPD all/sec", None, 8));
        }
        if on("backup") {
            post.push(c("BACKUP", None, 8));
        }
        if on("auto") {
            post.push(c("AUTO", None, 8));
        }
        let n = pre.len() + 1 + post.len();
        let used: usize = pre.iter().chain(post.iter()).map(|x| x.w).sum();
        let rest = (width as usize).saturating_sub(2 + used + 2 * (n - 1));
        let name_full = cw("NAME", 8).max(4);
        let mut name = name_full.min(rest).max(4);
        let mut tags_w = 0;
        if on("tags") {
            let tags_full = cw("TAGS", 8).max(4);
            // El hueco de TAGS cuesta 2 columnas más; NAME conserva su ancho completo.
            let avail = rest.saturating_sub(2 + name_full);
            if avail >= tags_full.min(6) {
                tags_w = tags_full.min(avail);
                name = name_full;
            }
        }
        let mut v = pre;
        v.push(c("NAME", Some(name), 8));
        // TAGS va antes de GPU/UPD/AUTO: se inserta tras DISK (o la última columna de datos).
        let at = post.iter().position(|x| matches!(x.title, "GPU" | "UPD all/sec" | "BACKUP" | "AUTO")).unwrap_or(post.len());
        if tags_w > 0 {
            post.insert(at, Col { title: "TAGS", w: tags_w });
        }
        v.extend(post);
        v
    } else {
        let mut v = vec![c("ID", None, 6), c("NAME", Some(0), 6), c("STATE", None, 6)];
        if gpu && on("gpu") {
            v.push(c("GPU", None, 6));
        } else if on("os") {
            v.push(c("OS", None, 6));
        }
        for (k, t) in [("cpu", "CPU"), ("ram", "RAM"), ("upd", "UPD"), ("backup", "BACKUP"), ("auto", "AUTO")] {
            if on(k) {
                v.push(c(t, if t == "CPU" { Some(5) } else { None }, 6));
            }
        }
        let used: usize = v.iter().map(|x| x.w).sum();
        let avail = (width as usize).saturating_sub(2 + used + 2 * (v.len() - 1));
        v[1].w = cw("NAME", 6).max(4).min(avail).max(4);
        v
    }
}

fn state_label(g: &Guest) -> &'static str {
    if g.lock.is_some() {
        return "locked";
    }
    match g.state {
        RunState::Running => "run",
        RunState::Stopped => "stopped",
        RunState::Remote => "remote",
    }
}

pub fn cpu_label(app: &App, g: &Guest) -> String {
    match app.cpu_pct.get(&g.vmid) {
        Some(p) => format!("{p:.1}%"),
        None if app.cpu_waiting && (g.kind == GuestKind::Lxc && g.is_running() || g.kind == GuestKind::Host && g.cpu_usec.is_some()) => "…".into(),
        None => "—".into(),
    }
}

fn cell(g: &Guest, app: &App, title: &str, w: usize) -> String {
    match title {
        "ID" => g.vmid.to_string(),
        "TYPE" => g.kind.label().into(),
        "NAME" => g.name.clone(),
        "STATE" => state_label(g).into(),
        "OS" => g.os_label.clone(),
        "CPU" => cpu_label(app, g),
        "RAM" => fmt::ram(g.mem_used_bytes, g.memory_mb),
        "DISK" => g.disk_bytes.map(fmt::bytes_short).unwrap_or_else(|| "—".into()),
        "TAGS" => g.tags.join(","),
        "UPD all/sec" | "UPD" => fmt::upd(g.updates(), g.security(), g.check.is_some()),
        "GPU" => hostcheck::gpu_label(g, app.inv.host.as_ref()).map_or("—".into(), |l| match l.text.strip_prefix("NV ") {
            // 80 columnas (GPU de 6): se queda `NV`; en la tabla ancha (GPU de 8) se escribe NVIDIA entero.
            Some(rest) if w >= 8 => format!("NVIDIA {rest}"),
            _ => l.text,
        }),
        "AUTO" => {
            if g.kind == GuestKind::Vm {
                "—".into()
            } else if g.auto_update {
                "yes".into()
            } else {
                "—".into()
            }
        }
        // `yes` = pxu hace backup antes de actualizar (auto/snapshot/vzdump); `-` = ninguno (none), VM y host. El modo exacto está en el detalle y en `b`.
        "BACKUP" => backup_cell(g.kind, &g.backup).into(),
        _ => String::new(),
    }
}

fn row_line(g: &Guest, app: &App, cols: &[Col], th: &Theme, selected: bool, w: usize) -> Line<'static> {
    let mut spans = vec![Span::raw(if selected { "> " } else { "  " })];
    for (i, c) in cols.iter().enumerate() {
        let text = trunc(&cell(g, app, c.title, if w >= 112 { 8 } else { 6 }), c.w);
        let pad = " ".repeat(c.w.saturating_sub(text.chars().count()));
        let right = matches!(c.title, "ID" | "CPU");
        let body = if right { format!("{pad}{text}") } else { format!("{text}{pad}") };
        let style = match c.title {
            "TYPE" if g.kind == GuestKind::Vm => th.vm(),
            "GPU" => match hostcheck::gpu_label(g, app.inv.host.as_ref()) {
                Some(l) if l.warn => th.warn(),
                Some(_) => Style::default(),
                None => th.muted(),
            },
            "TYPE" => th.accent(),
            "STATE" => match state_label(g) {
                "run" => th.ok(),
                "locked" => th.warn(),
                _ => th.muted(),
            },
            "UPD all/sec" | "UPD" => {
                if g.security() > 0 {
                    th.warn()
                } else if g.updates() > 0 {
                    th.accent()
                } else {
                    th.muted()
                }
            }
            "AUTO" if g.auto_update => th.ok(),
            "AUTO" => th.muted(),
            "BACKUP" if g.kind == GuestKind::Lxc && g.backup == "none" => th.warn(),
            "BACKUP" => th.muted(),
            _ if g.state != RunState::Running && matches!(c.title, "NAME" | "OS" | "CPU" | "RAM" | "DISK" | "TAGS") => th.muted(),
            _ => Style::default(),
        };
        spans.push(Span::styled(body, style));
        if i + 1 < cols.len() {
            spans.push(Span::raw("  "));
        }
    }
    let mut line = Line::from(spans);
    if selected {
        // La selección en video inverso pisa colores; el `>` cubre NO_COLOR. Cubre todo el ancho de la pantalla.
        let used: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
        line.spans.push(Span::raw(" ".repeat(w.saturating_sub(used))));
        line.style(th.sel())
    } else {
        line
    }
}

fn draw_list(f: &mut Frame, app: &App, th: &Theme, area: Rect) {
    let w = area.width;
    // Teclas: una sola línea si caben; si no, dos (la fila sobrante va a la tabla).
    let keys = if w < 112 {
        keybar_wrap(th, &[("↑↓", "move"), ("1-4", "filter"), ("/", "search"), ("s", "sort"), ("c", "check"), ("u", "update"), ("r", "reload"), ("a", "auto"), ("b", "bkp"), ("o", "cols"), ("g", "cfg"), ("d", "space"), ("l", "logs"), ("S", "schedule"), ("?", "help"), ("q", "quit")], 2, w as usize, 2)
    } else {
        keybar_wrap(th, &[("↑↓/jk", "move"), ("1-4", "filter"), ("/", "search"), ("s", "sort"), ("c", "check"), ("u", "update (asks)"), ("r", "reload"), ("a", "auto"), ("b", "backup"), ("o", "columns"), ("d", "storage"), ("l", "logs"), ("g", "config"), ("S", "schedule"), ("?", "help"), ("q", "quit")], 2, w as usize, 2)
    };
    let parts = Layout::vertical([
        Constraint::Length(1), // header
        Constraint::Length(1), // filtros
        Constraint::Length(1), // regla
        Constraint::Length(1), // encabezado de columnas
        Constraint::Min(1),    // filas
        Constraint::Length(1), // indicador de scroll
        Constraint::Length(1), // regla
        Constraint::Length(1), // línea de estado
        Constraint::Length(1), // regla
        Constraint::Length(keys.len() as u16), // teclas
    ])
    .split(area);

    let inv = &app.inv;
    // Filters and totals count guests only; the host row is summarised apart.
    let host_part = match inv.host.as_ref() {
        Some(h) if h.error.is_none() => format!(" · host: {} upd", h.updates()),
        Some(_) => " · host: error".to_string(),
        None => String::new(),
    };
    let summary = if w >= 112 {
        format!("node {} · {} guests · {} running · {} with updates{}   {}", inv.node, inv.count(), inv.running(), inv.with_updates(), host_part, app.clock)
    } else {
        format!("{} · {}/{} up · {} upd{}   {}", inv.node, inv.running(), inv.count(), inv.with_updates(), host_part, app.clock)
    };
    let title = if w >= 112 { "pxu · Proxmox Updater" } else { "pxu" };
    f.render_widget(
        Paragraph::new(two_sided(vec![Span::styled(title.to_string(), th.bold()), Span::styled(badges(app), th.warn())], summary, w)),
        parts[0],
    );

    // Barra de filtros con contadores; el activo entre corchetes + acento.
    let mut fs: Vec<Span> = Vec::new();
    let narrow = w < 112;
    for (flt, name, short) in [(Filter::All, "All", "All"), (Filter::Running, "Running", "Run"), (Filter::Stopped, "Stopped", "Stop"), (Filter::Updates, "Updates", "Upd")] {
        let label = format!("{} {}", if narrow { short } else { name }, app.count(flt));
        if flt == app.filter {
            fs.push(Span::styled(format!("[{label}]"), th.accent().add_modifier(Modifier::BOLD)));
        } else {
            fs.push(Span::raw(format!(" {label} ")));
        }
        fs.push(Span::raw(if narrow { " " } else { "  " }));
    }
    let search = if app.searching {
        format!("/ {}_", app.search)
    } else if !app.search.is_empty() {
        format!("/ {}", app.search)
    } else if narrow {
        "/ search…".to_string()
    } else {
        "/ search name, tag, id…".to_string()
    };
    fs.push(Span::styled(search, if app.search.is_empty() && !app.searching { th.muted() } else { th.accent() }));
    let sort = format!("sort: {}", app.sort.label());
    let sort = if narrow { String::new() } else { sort };
    f.render_widget(Paragraph::new(two_sided(fs, sort, w)), parts[1]);
    f.render_widget(Paragraph::new("─".repeat(w as usize)).style(th.muted()), parts[2]);

    let cw = |title: &str, gw: usize| app.inv.guests.iter().map(|g| cell(g, app, title, gw).chars().count()).max().unwrap_or(0);
    let cols = columns(w, app.inv.any_gpu(), &app.columns, &cw);
    let mut head = vec![Span::raw("  ")];
    for (i, c) in cols.iter().enumerate() {
        let t = trunc(c.title, c.w);
        let pad = " ".repeat(c.w.saturating_sub(t.chars().count()));
        head.push(Span::styled(if matches!(c.title, "ID" | "CPU") { format!("{pad}{t}") } else { format!("{t}{pad}") }, th.bold().patch(th.muted())));
        if i + 1 < cols.len() {
            head.push(Span::raw("  "));
        }
    }
    f.render_widget(Paragraph::new(Line::from(head)).style(th.bold()), parts[3]);

    // Filas con ventana deslizante alrededor del cursor.
    let vis = app.visible();
    let h = parts[4].height as usize;
    let off = if app.cursor >= h { app.cursor + 1 - h } else { 0 };
    let lines: Vec<Line> = vis.iter().enumerate().skip(off).take(h).map(|(i, g)| row_line(g, app, &cols, th, i == app.cursor, w as usize)).collect();
    if lines.is_empty() {
        f.render_widget(Paragraph::new("  no guests match this filter").style(th.muted()), parts[4]);
    } else {
        f.render_widget(Paragraph::new(lines), parts[4]);
    }

    let above = off;
    let below = vis.len().saturating_sub(off + h);
    let mut ind = String::new();
    if above > 0 {
        ind.push_str(&format!("▲ {above} above"));
        if let Some(g) = vis.get(off) {
            ind.push_str(&format!(" (first {} {})", g.vmid, g.name));
        }
        ind.push_str("   ");
    }
    if below > 0 {
        ind.push_str(&format!("▼ {below} below"));
        if let Some(g) = vis.get(off + h) {
            ind.push_str(&format!(" (next {} {})", g.vmid, g.name));
        }
        ind.push_str("   ");
    }
    if !ind.is_empty() || app.filter != Filter::All {
        ind.push_str(&format!("— filter: {:?} · {} guests", app.filter, vis.iter().filter(|g| g.kind != GuestKind::Host).count()).to_lowercase());
    }
    if let Some(r) = &app.ro_reason {
        ind = format!("read-only: {r}   {ind}");
    }
    f.render_widget(Paragraph::new(trunc(ind.trim_end(), w as usize)).style(th.muted()), parts[5]);

    // Línea de estado: guest bajo el cursor (o mensaje puntual).
    let st = if !app.status.is_empty() {
        app.status.clone()
    } else if let Some(g) = app.selected() {
        let upd = match (&g.check, g.kind) {
            (_, GuestKind::Vm) => "not checked (VM)".to_string(),
            (None, GuestKind::Host) => format!("host review failed: {}", app.inv.host.as_ref().and_then(|h| h.error.clone()).unwrap_or_default()),
            (None, _) => "no check data".to_string(),
            (Some(_), _) => format!("{} updates ({} security)", g.updates(), g.security()),
        };
        format!("> {} {} · {} · {} · auto: {} · [Enter] detail", g.vmid, g.name, g.os_label, upd, if g.auto_update { "yes" } else { "no" })
    } else {
        String::new()
    };
    f.render_widget(Paragraph::new(trunc(&st, w as usize)).style(th.accent()), parts[7]);
    f.render_widget(Paragraph::new("─".repeat(w as usize)).style(th.muted()), parts[6]);
    f.render_widget(Paragraph::new("─".repeat(w as usize)).style(th.muted()), parts[8]);

    f.render_widget(Paragraph::new(keys), parts[9]);
}

// ---------------------------------------------------------------- detalle

/// "8G" → "8.0 GB"; si no se entiende, el valor tal cual.
fn size(o: Option<&str>) -> String {
    match o {
        Some(v) => parse_size(v).map(fmt::bytes_short).unwrap_or_else(|| v.to_string()),
        None => "—".into(),
    }
}

fn dash(o: Option<&str>) -> String {
    o.filter(|s| !s.is_empty()).unwrap_or("—").to_string()
}

fn ago(now: u64, then: u64) -> String {
    let d = now.saturating_sub(then);
    match d {
        0..=89 => "1 min ago".into(),
        90..=5399 => format!("{} min ago", d / 60),
        5400..=172799 => format!("{} h ago", d / 3600),
        _ => format!("{} d ago", d / 86400),
    }
}

fn age(s: u64) -> String {
    match s {
        0..=5399 => format!("{} min", s / 60),
        5400..=172799 => format!("{} h", s / 3600),
        _ => format!("{} d", s / 86400),
    }
}

/// Opciones de un valor `vol,k=v,…` que no se muestran ya en otra parte, como `k=v` compactas.
fn extra_props(p: &[(&str, &str)], skip: &[&str]) -> String {
    p.iter().filter(|(k, _)| !skip.contains(k)).map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(" ")
}

/// Claves cuyo valor nunca se muestra en claro.
fn is_secret_key(k: &str) -> bool {
    let k = k.to_ascii_lowercase();
    k.contains("password") || k.contains("secret") || k.contains("token")
}

fn decode_entities(s: &str) -> String {
    s.replace("&nbsp;", " ").replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&#39;", "'").replace("&#x27;", "'").replace("&amp;", "&")
}

/// Valor de un atributo (`href='…'`, `alt="…"`) dentro del texto de una etiqueta.
fn attr(tag: &str, name: &str) -> Option<String> {
    let at = tag.find(&format!("{name}="))? + name.len() + 1;
    let rest = &tag[at..];
    let q = rest.chars().next().filter(|c| *c == '\'' || *c == '"')?;
    let end = rest[1..].find(q)?;
    Some(rest[1..1 + end].to_string())
}

/// Descripción de PVE (ya decodificada de `%XX`) → líneas legibles. Si es HTML, quita las
/// etiquetas, deja el texto y la URL de cada enlace; si queda vacía, `(html, N lines)`.
pub fn description_lines(desc: &[String]) -> Vec<String> {
    let joined = desc.join("\n");
    if !(joined.contains('<') && joined.contains('>')) {
        let mut v: Vec<String> = desc.iter().map(|l| l.trim_end().to_string()).collect();
        while v.last().is_some_and(|l| l.is_empty()) {
            v.pop();
        }
        return v;
    }
    let mut out = String::new();
    let mut href: Option<String> = None;
    let mut link_text = String::new();
    let mut rest = joined.as_str();
    while let Some(i) = rest.find('<') {
        let (text, after) = rest.split_at(i);
        out.push_str(text);
        link_text.push_str(text);
        let Some(j) = after.find('>') else {
            out.push_str(after);
            rest = "";
            break;
        };
        let tag = &after[1..j];
        let name = tag.trim_start_matches('/').split(|c: char| c.is_whitespace() || c == '/').next().unwrap_or("").to_ascii_lowercase();
        let closing = tag.starts_with('/');
        match name.as_str() {
            "a" if !closing => {
                href = attr(tag, "href");
                link_text.clear();
            }
            "a" => {
                if let Some(h) = href.take() {
                    let t = link_text.split_whitespace().collect::<Vec<_>>().join(" ");
                    if t.is_empty() || t == h {
                        out.push_str(&format!(" {h} "));
                    } else {
                        out.push_str(&format!(" ({h}) "));
                    }
                }
            }
            "img" => {
                if let Some(alt) = attr(tag, "alt").filter(|a| !a.is_empty()) {
                    out.push_str(&alt);
                    link_text.push_str(&alt);
                }
            }
            "br" | "div" | "p" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "li" | "ul" | "ol" | "hr" | "table" | "tr" => out.push('\n'),
            _ => {}
        }
        rest = &after[j + 1..];
    }
    out.push_str(rest);
    let lines: Vec<String> = decode_entities(&out).lines().map(|l| l.split_whitespace().collect::<Vec<_>>().join(" ")).filter(|l| !l.is_empty()).collect();
    if lines.is_empty() {
        return vec![format!("(html, {} lines)", desc.len())];
    }
    lines
}

/// Parte `s` en filas de ancho `w`; las continuaciones llevan la sangría de la línea más 2 espacios.
fn wrap_line(s: &str, w: usize) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    if w < 8 || chars.len() <= w {
        return vec![s.to_string()];
    }
    let indent = s.chars().take_while(|c| *c == ' ').count() + 2;
    let indent = indent.min(w / 2);
    let mut rows: Vec<String> = Vec::new();
    let mut start = 0;
    while start < chars.len() {
        let room = if rows.is_empty() { w } else { w - indent };
        let pad = if rows.is_empty() { String::new() } else { " ".repeat(indent) };
        if chars.len() - start <= room {
            rows.push(format!("{pad}{}", chars[start..].iter().collect::<String>()));
            break;
        }
        let limit = start + room;
        let cut = chars[start + 1..limit].iter().rposition(|c| *c == ' ' || *c == ',').map(|p| start + 1 + p + 1).filter(|c| *c > start + room / 3).unwrap_or(limit);
        rows.push(format!("{pad}{}", chars[start..cut].iter().collect::<String>().trim_end()));
        start = cut;
        while start < chars.len() && chars[start] == ' ' {
            start += 1;
        }
    }
    rows
}

/// Envuelve cada línea a `w` columnas para que ninguna se corte sin poder leerla con scroll.
fn wrap_src(src: &[(String, bool)], w: usize) -> Vec<(String, bool)> {
    src.iter().flat_map(|(s, h)| wrap_line(s, w).into_iter().map(move |r| (r, *h))).collect()
}

/// Líneas de configuración (columna izquierda): `(texto, es_título_de_sección)`.
/// Toda clave del .conf aparece: las conocidas en su sección y el resto en «other».
pub fn config_lines(g: &Guest) -> Vec<(String, bool)> {
    use std::cell::RefCell;
    use std::collections::HashSet;
    let c = &g.conf;
    let used: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
    // Toda lectura de una clave pasa por aquí: así «other» no depende de una lista aparte.
    let get = |k: &str| {
        used.borrow_mut().insert(k.to_string());
        c.get(k)
    };
    let indexed = |p: &str| {
        let r = c.indexed(p);
        let mut u = used.borrow_mut();
        for (k, _) in &r {
            u.insert(k.to_string());
        }
        r
    };
    let mut v: Vec<(String, bool)> = Vec::new();
    let state = match g.state {
        RunState::Running => "running",
        RunState::Stopped => "stopped",
        RunState::Remote => "remote node (state n/a)",
    };
    // Mostradas desde `Guest`, no desde el .conf.
    for k in ["ostype", "hostname", "name", "tags", "lock", "template"] {
        get(k);
    }
    // Un campo ausente o vacío no se muestra; una línea sin campos tampoco; una sección sin líneas tampoco.
    let kv = |label: &str, o: Option<&str>| o.filter(|s| !s.is_empty()).map(|s| format!("{label} {s}"));
    let row = |indent: &str, parts: Vec<Option<String>>| {
        let p: Vec<String> = parts.into_iter().flatten().collect();
        if p.is_empty() { None } else { Some(format!("{indent}{}", p.join(" · "))) }
    };
    let flag = |s: &str| (!s.is_empty()).then(|| s.to_string());
    let tags = g.tags.join(",");
    let tmpl = Some(format!("template {}", if g.template { "yes" } else { "no" }));
    let mut head: Vec<Option<String>> = Vec::new();
    let mut secs: Vec<(String, Vec<String>)> = Vec::new();
    match g.kind {
        GuestKind::Lxc => {
            head.push(Some(format!("vmid {} · hostname {} · type LXC · node {}", g.vmid, g.name, g.node)));
            head.push(row("", vec![Some(format!("state {state}")), kv("ostype", Some(&g.ostype)), kv("arch", get("arch"))]));
            head.push(row("", vec![kv("unprivileged", get("unprivileged")), kv("onboot", get("onboot")), kv("startup", get("startup"))]));
            head.push(row("", vec![kv("protection", get("protection")), kv("lock", g.lock.as_deref()), kv("tags", flag(&tags).as_deref()), tmpl]));
            head.push(kv("hookscript", get("hookscript")));
            let mut s: Vec<String> = Vec::new();
            s.extend(row("  ", vec![kv("cores", get("cores")), kv("cpulimit", get("cpulimit")), kv("cpuunits", get("cpuunits"))]));
            let mem = get("memory").map(|m| format!("{m} MB"));
            let swap = get("swap").map(|m| format!("{m} MB"));
            s.extend(row("  ", vec![kv("memory", mem.as_deref()), kv("swap", swap.as_deref())]));
            s.extend(kv("  timezone", get("timezone")));
            if let Some(val) = get("rootfs") {
                let (vol, p) = parse_props(val);
                let sz = prop(&p, "size").map(|x| size(Some(x)));
                let ex = extra_props(&p, &["size"]);
                s.extend(row("  ", vec![kv("rootfs", vol), sz, flag(&ex)]));
            }
            for (k, val) in indexed("mp") {
                let (vol, p) = parse_props(val);
                let sz = prop(&p, "size").map(|x| size(Some(x)));
                let ex = extra_props(&p, &["mp", "size"]);
                let first = format!("{k} {}{}", dash(vol), prop(&p, "mp").map(|m| format!(" → {m}")).unwrap_or_default());
                s.extend(row("  ", vec![Some(first), sz, flag(&ex)]));
            }
            secs.push(("resources".into(), s));
            let mut s: Vec<String> = Vec::new();
            for (k, val) in indexed("net") {
                let (_, p) = parse_props(val);
                let f = |key: &str| prop(&p, key).filter(|x| !x.is_empty()).map(|x| format!("{key}={x}"));
                let first: Vec<String> = [Some(k.to_string()), f("name"), f("bridge"), f("ip"), f("firewall")].into_iter().flatten().collect();
                s.push(format!("  {}", first.join(" ")));
                s.extend(row("       ", vec![kv("hwaddr", prop(&p, "hwaddr")), kv("vlan", prop(&p, "tag")), kv("gw", prop(&p, "gw"))]));
                let ex = extra_props(&p, &["name", "bridge", "ip", "firewall", "hwaddr", "tag", "gw"]);
                if !ex.is_empty() {
                    s.push(format!("       {ex}"));
                }
            }
            secs.push(("network".into(), s));
            let mut s: Vec<String> = Vec::new();
            for (k, val) in indexed("dev") {
                let (path, p) = parse_props(val);
                s.extend(row("  ", vec![Some(format!("{k} {}", dash(path))), flag(&extra_props(&p, &[]))]));
            }
            secs.push(("devices".into(), s));
            let feat: Vec<(&str, &str)> = get("features").map(|f| parse_props(f).1).unwrap_or_default();
            let fv = |k: &str| kv(k, prop(&feat, k));
            let s: Vec<String> = row("  ", vec![fv("nesting"), fv("keyctl"), fv("fuse"), fv("mount")]).into_iter().collect();
            secs.push(("features".into(), s));
        }
        // La fila del host no tiene .conf: su detalle se arma en `host_lines`.
        GuestKind::Host => {}
        GuestKind::Vm => {
            head.push(Some(format!("vmid {} · name {} · type VM · node {}", g.vmid, g.name, g.node)));
            head.push(row("", vec![Some(format!("state {state}")), kv("ostype", Some(&g.ostype)), kv("machine", get("machine"))]));
            head.push(row("", vec![kv("bios", get("bios")), kv("agent", get("agent")), kv("onboot", get("onboot")), kv("startup", get("startup"))]));
            head.push(row("", vec![kv("protection", get("protection")), kv("lock", g.lock.as_deref()), kv("tags", flag(&tags).as_deref()), tmpl]));
            head.push(kv("hookscript", get("hookscript")));
            let mut s: Vec<String> = Vec::new();
            let topo = match (get("sockets").filter(|x| !x.is_empty()), get("cores").filter(|x| !x.is_empty())) {
                (Some(so), Some(co)) => Some(format!("sockets {so} × cores {co}")),
                (Some(so), None) => Some(format!("sockets {so}")),
                (None, Some(co)) => Some(format!("cores {co}")),
                _ => None,
            };
            s.extend(row("  ", vec![kv("cpu", get("cpu")), topo, kv("numa", get("numa"))]));
            let bal = match get("balloon") {
                Some("0") => Some("0 (disabled)"),
                o => o,
            };
            let mem = get("memory").map(|m| format!("{m} MB"));
            s.extend(row("  ", vec![kv("memory", mem.as_deref()), kv("balloon", bal)]));
            secs.push(("cpu & memory".into(), s));
            let mut s: Vec<String> = Vec::new();
            for (k, val) in &c.main {
                let base = k.trim_end_matches(|ch: char| ch.is_ascii_digit());
                if matches!(base, "scsi" | "virtio" | "sata" | "ide" | "efidisk" | "tpmstate" | "unused") && k.len() > base.len() {
                    used.borrow_mut().insert(k.clone());
                    let (vol, p) = parse_props(val);
                    let sz = prop(&p, "size").map(|x| size(Some(x)));
                    let extra: Vec<String> = ["ssd", "discard", "iothread", "cache"].iter().filter_map(|key| prop(&p, key).map(|x| format!("{key}={x}"))).collect();
                    s.extend(row("  ", vec![Some(format!("{k} {}", dash(vol))), sz, flag(&extra.join(" "))]));
                }
            }
            let hw = get("scsihw").filter(|x| !x.is_empty()).map(|x| format!(" (scsihw {x})")).unwrap_or_default();
            secs.push((format!("disks{hw}"), s));
            let mut s: Vec<String> = Vec::new();
            for (k, val) in indexed("net") {
                s.push(format!("  {k} {val}"));
            }
            for (k, val) in indexed("hostpci").into_iter().chain(indexed("usb")) {
                s.push(format!("  {k} {val}"));
            }
            secs.push(("network & passthrough".into(), s));
            secs.push(("boot".into(), get("boot").filter(|b| !b.is_empty()).map(|b| format!("  {b}")).into_iter().collect()));
            let mut ci: Vec<String> = Vec::new();
            for (k, val) in indexed("ipconfig") {
                ci.push(format!("  {k} {val}"));
            }
            for k in ["cicustom", "ciuser", "cipassword", "citype", "ciupgrade", "sshkeys", "nameserver", "searchdomain"] {
                if let Some(val) = get(k) {
                    ci.push(format!("  {k} {}", if is_secret_key(k) { "•••".to_string() } else { val.to_string() }));
                }
            }
            secs.push(("cloud-init".into(), ci));
        }
    }
    let mut s: Vec<String> = Vec::new();
    let sn = &c.snapshots;
    s.push(format!("  snapshots {}{}", sn.len(), if sn.is_empty() { String::new() } else { format!(" ({})", sn.join(", ")) }));
    if c.pending.is_empty() {
        s.push("  pending changes: none".into());
    } else {
        s.push("  pending changes:".into());
        for (k, val) in &c.pending {
            s.push(format!("    {k}: {}", if is_secret_key(k) { "•••" } else { val }));
        }
    }
    if !c.description.is_empty() {
        s.push("  description:".into());
        for d in description_lines(&c.description) {
            s.push(format!("    {d}"));
        }
    }
    for (k, val) in c.raw_lxc() {
        s.push(format!("  {k}: {val}"));
    }
    secs.push(("ops".into(), s));
    for (k, _) in c.main.iter().filter(|(k, _)| k.starts_with("lxc.")) {
        used.borrow_mut().insert(k.clone());
    }
    // Toda clave que ninguna sección mostró, en el orden del archivo.
    let other: Vec<String> = c
        .main
        .iter()
        .filter(|(k, _)| !used.borrow().contains(k))
        .map(|(k, val)| format!("  {k}: {}", if is_secret_key(k) { "•••" } else { val }))
        .collect();
    secs.push(("other".into(), other));
    for line in head.into_iter().flatten() {
        v.push((line, false));
    }
    for (title, lines) in secs {
        if lines.is_empty() {
            continue;
        }
        v.push((String::new(), false));
        v.push((title, true));
        v.extend(lines.into_iter().map(|s| (s, false)));
    }
    v
}

/// Detalle del host (columna izquierda): paquetes agrupados, repos, GPUs y CTs con NVIDIA. Sin campos ausentes ni secciones vacías.
fn host_lines(h: &HostInfo, inv: &pxu::share::proxmox::guests::Inventory) -> Vec<(String, bool)> {
    let mut v: Vec<(String, bool)> = Vec::new();
    let mut sec = |title: String, lines: Vec<String>| {
        if lines.is_empty() {
            return;
        }
        if !v.is_empty() {
            v.push((String::new(), false));
        }
        v.push((title, true));
        v.extend(lines.into_iter().map(|s| (s, false)));
    };
    if let Some(e) = &h.error {
        sec("host review failed".into(), vec![format!("  {e}")]);
        return v;
    }
    for group in ["kernel", "firmware", "pve", "security", "other"] {
        let p: Vec<&pxu::share::proxmox::hostcheck::HostPkg> = h.packages.iter().filter(|p| p.group() == group).collect();
        sec(format!("{group} ({})", p.len()), p.iter().map(|p| format!("  {} {} → {}{}", p.name, p.cur, p.new, if p.sec && group != "security" { " (security)" } else { "" })).collect());
    }
    sec(
        "repos".into(),
        h.repos.iter().map(|r| format!("  {} · {} {} {}{}", r.label(), r.uri, r.suites, r.components, if r.enabled { "" } else { " (disabled)" })).collect(),
    );
    sec(
        "GPUs".into(),
        h.gpus.iter().map(|g| format!("  {} {} · {}", g.vendor.long(), g.slot, g.name)).collect(),
    );
    if h.has_nvidia() {
        let mut n: Vec<String> = Vec::new();
        n.push(format!("  driver {} · module for {}: {}", h.nvidia_driver.as_deref().unwrap_or("not loaded"), if h.newest_kernel.is_empty() { "newest kernel" } else { &h.newest_kernel }, if h.nvidia_missing_for.is_none() { "yes" } else { "MISSING" }));
        let on = inv.guests.iter().any(|g| g.kind == GuestKind::Host && g.nvidia_auto);
        n.push(format!("  driver update: {} (n)", if on { "in scheduled updates" } else { "not in updates" }));
        for d in h.dkms.iter().filter(|d| d.module.starts_with("nvidia")) {
            n.push(format!("  dkms {} {} · {} · {}", d.module, d.version, d.kernel, d.status));
        }
        for (id, c) in &h.ct_nvidia {
            let name = inv.guests.iter().find(|g| g.vmid.to_string() == *id).map(|g| g.name.as_str()).unwrap_or("");
            let state = inv.guests.iter().find(|g| g.vmid.to_string() == *id).map_or("", |g| if g.is_running() { "running" } else { "stopped" });
            let ver = match (&c.version, c.mismatch) {
                (_, true) => "version mismatch".to_string(),
                (Some(x), _) => format!("{x}{}", if h.nvidia_driver.as_deref() == Some(x) { " (= host)" } else { " (≠ host)" }),
                (None, _) => "unknown".to_string(),
            };
            n.push(format!("  CT {id} {name} · {state} · NVIDIA {ver}"));
        }
        sec("NVIDIA".into(), n);
    }
    v
}

fn host_state_lines(h: &HostInfo, app: &App) -> Vec<(String, bool)> {
    let mut v: Vec<(String, bool)> = Vec::new();
    v.push((format!("host review · {} · read-only", ago(app.now_epoch, h.checked_at)), true));
    if let Some(e) = &h.error {
        v.push((format!("  error: {e}"), false));
        return v;
    }
    v.push((format!("  {} updates · {} security", h.updates(), h.security()), false));
    if h.reboot_pending() {
        v.push((format!("  reboot pending: {}", h.reboot_reason), false));
    }
    if let Some(b) = &h.blocked {
        v.push((format!("  BLOCKED: {b}"), false));
    }
    if !h.running_kernel.is_empty() {
        v.push((format!("  kernel running {}", h.running_kernel), false));
    }
    if !h.newest_kernel.is_empty() && h.newest_kernel != h.running_kernel {
        v.push((format!("  kernel installed {}", h.newest_kernel), false));
    }
    for w in &h.repo_warnings {
        v.push((format!("  ! {w}"), false));
    }
    for n in &h.notes {
        v.push((format!("  note: {n}"), false));
    }
    v.push((String::new(), false));
    v.push(("install".into(), true));
    v.push(("  press u: review, confirm and install (pxu update --host)".into(), false));
    v
}

fn state_lines(g: &Guest, app: &App) -> Vec<(String, bool)> {
    let mut v: Vec<(String, bool)> = Vec::new();
    let mut l = |s: String, h: bool| v.push((s, h));
    let st = match g.state {
        RunState::Running => "running",
        RunState::Stopped => "stopped",
        RunState::Remote => "remote node",
    };
    l(format!("{st}{}", if g.lock.is_some() { format!(" · locked ({})", g.lock.as_deref().unwrap_or("")) } else { String::new() }), false);
    let mem = fmt::ram(g.mem_used_bytes, g.memory_mb);
    l(format!("cpu {} · mem {mem}", cpu_label(app, g)), false);
    l(format!("disk allocated {}", g.disk_bytes.map(fmt::bytes_short).unwrap_or_else(|| "—".into())), false);
    if g.kind == GuestKind::Lxc {
        let ip = g.conf.indexed("net").iter().find_map(|(k, v)| {
            let (_, p) = parse_props(v);
            prop(&p, "ip").map(|ip| format!("{ip} ({k})"))
        });
        l(format!("ip {}", ip.unwrap_or_else(|| "—".into())), false);
    }
    if let Some(t) = hostcheck::gpu_detail(g, app.inv.host.as_ref()) {
        l(format!("gpu: {t}"), false);
    }
    if g.kind == GuestKind::Lxc && g.gpu.nvidia {
        l(format!("gpu driver: {} (a)", if g.nvidia_auto { "in scheduled updates" } else { "not in updates" }), false);
    }
    l(String::new(), false);
    match (&g.check, g.kind) {
        (_, GuestKind::Vm) => {
            l("updates".into(), true);
            l("  — pxu checks and updates LXC only".into(), false);
            l("  nothing is run inside VMs".into(), false);
        }
        (None, _) => {
            l("check".into(), true);
            l("  no check data yet (check arrives in M2)".into(), false);
        }
        (Some(c), _) => {
            l(format!("check · {} · read-only", ago(app.now_epoch, c.checked_at)), true);
            l(format!("pm {} ({}) · index age {}", c.pm, dash(Some(&c.os)), age(c.index_age_s)), false);
            let sec = c.packages.iter().filter(|p| p.sec).count();
            l(format!("{} updates · {} security · reboot: {}", c.packages.len(), sec, if c.reboot_required { "yes" } else { "no" }), false);
            l(String::new(), false);
            l(format!("packages ({})", c.packages.len()), true);
            for p in &c.packages {
                l(format!("  {} {} → {}{}", p.name, p.cur, p.new, if p.sec { "  [sec]" } else { "" }), false);
            }
            if !c.held.is_empty() {
                l(format!("held: {}", c.held.join(", ")), false);
            }
        }
    }
    if g.kind == GuestKind::Lxc {
        l(String::new(), false);
        let why = if g.auto_update { "yes (config/tag)" } else { "no" };
        l(format!("auto-update: {why}"), false);
        l(format!("backup before update: {}", if g.backup == "none" { "none (no rollback)".to_string() } else { format!("yes, mode {}", g.backup) }), false);
    }
    v
}

fn to_lines(src: &[(String, bool)], th: &Theme, w: usize, skip: usize, h: usize) -> Vec<Line<'static>> {
    src.iter()
        .skip(skip)
        .take(h)
        .map(|(s, head)| {
            let t = trunc(s, w);
            if *head {
                Line::styled(t, th.bold().patch(th.accent()))
            } else if s.contains("[sec]") {
                Line::styled(t, th.warn())
            } else {
                Line::raw(t)
            }
        })
        .collect()
}

fn draw_detail(f: &mut Frame, app: &App, th: &Theme, area: Rect, id: u32) {
    let w = area.width;
    let Some(g) = app.guest(id) else {
        f.render_widget(Paragraph::new("guest vanished (press Esc)"), area);
        return;
    };
    let mut keys = vec![("↑↓/jk", "scroll"), ("r", "reload"), ("a", "auto"), ("b", "backup"), ("g", "config"), ("S", "schedule"), ("Esc", "back"), ("?", "help"), ("q", "quit")];
    if g.kind != GuestKind::Vm {
        keys.insert(1, ("u", "update (asks)"));
    }
    let keys = keybar_wrap(th, &keys, 2, w as usize, 2);
    let parts = Layout::vertical([Constraint::Length(1), Constraint::Length(1), Constraint::Length(1), Constraint::Min(3), Constraint::Length(1), Constraint::Length(1), Constraint::Length(1), Constraint::Length(keys.len() as u16 + 1)]).split(area);

    let st = match g.state {
        RunState::Running => "running",
        RunState::Stopped => "stopped",
        RunState::Remote => "remote",
    };
    f.render_widget(
        Paragraph::new(two_sided(
            vec![Span::styled(format!("pxu · guest {} · {}", g.vmid, trunc(&g.name, 30)), th.bold()), Span::styled(badges(app), th.warn())],
            format!("{} · {} · {st}   {}", g.kind.label(), g.node, app.clock),
            w,
        )),
        parts[0],
    );
    let sub = match g.kind {
        GuestKind::Lxc => "detail · config (left) · state & updates (right)",
        GuestKind::Vm => "detail · config (left) · state (right) · pxu checks and updates LXC only — VMs are listed for reference",
        GuestKind::Host => "detail · host review · u installs host updates after a confirmation",
    };
    f.render_widget(Paragraph::new(trunc(sub, w as usize)).style(th.muted()), parts[1]);
    f.render_widget(Paragraph::new("─".repeat(w as usize)).style(th.muted()), parts[2]);

    let body = parts[3];
    let two = w >= 100;
    // Ancho útil de cada panel (bordes fuera); las líneas largas se envuelven, no se cortan.
    let (lw, rw) = if two { ((w as usize * 59 / 100).saturating_sub(3), (w as usize - w as usize * 59 / 100).saturating_sub(3)) } else { ((w as usize).saturating_sub(2), (w as usize).saturating_sub(2)) };
    let is_host = g.kind == GuestKind::Host;
    let (cl, sl) = match (is_host, app.inv.host.as_ref()) {
        (true, Some(h)) => (host_lines(h, &app.inv), host_state_lines(h, app)),
        _ => (config_lines(g), state_lines(g, app)),
    };
    let left = wrap_src(&cl, lw);
    let right = wrap_src(&sl, rw);
    let (title_l, title_r) = if is_host {
        (" host · packages, repos, GPUs ".to_string(), " review ".to_string())
    } else {
        (format!(" config · {} ", g.conf_path), if g.kind == GuestKind::Lxc { " state & updates ".to_string() } else { " state ".to_string() })
    };
    if two {
        let cols = Layout::horizontal([Constraint::Percentage(59), Constraint::Percentage(41)]).split(body);
        let inner_h = body.height.saturating_sub(2) as usize;
        let max_scroll = left.len().max(right.len()).saturating_sub(inner_h);
        let sc = app.detail_scroll.min(max_scroll);
        for (area, title, src) in [(cols[0], title_l, &left), (cols[1], title_r, &right)] {
            let blk = Block::default().borders(Borders::ALL).title(Span::styled(trunc(&title, area.width.saturating_sub(4) as usize), th.bold())).border_style(th.muted());
            let inner = blk.inner(area);
            f.render_widget(blk, area);
            f.render_widget(Paragraph::new(to_lines(src, th, inner.width as usize, sc, inner.height as usize)), inner);
        }
    } else {
        // Terminal estrecha: un solo panel, config y estado apilados, con scroll.
        let mut all = left.clone();
        all.push((String::new(), false));
        all.push((if g.kind == GuestKind::Lxc { "state & updates".into() } else { "state".into() }, true));
        all.extend(right);
        let blk = Block::default().borders(Borders::ALL).title(Span::styled(trunc(&title_l, w.saturating_sub(4) as usize), th.bold())).border_style(th.muted());
        let inner = blk.inner(body);
        let max_scroll = all.len().saturating_sub(inner.height as usize);
        let sc = app.detail_scroll.min(max_scroll);
        f.render_widget(blk, body);
        f.render_widget(Paragraph::new(to_lines(&all, th, inner.width as usize, sc, inner.height as usize)), inner);
    }

    let is_host = g.kind == GuestKind::Host;
    let hint = if !app.status.is_empty() { app.status.clone() } else if is_host { format!("{} · u installs host updates after confirmation · a: scheduled run {}", g.name, if g.auto_update { "updates the host FIRST (host_auto on)" } else { "never touches it (host_auto off)" }) } else { format!("{} · read from {}", g.name, g.conf_path) };
    f.render_widget(Paragraph::new(trunc(&hint, w as usize)).style(th.accent()), parts[5]);
    f.render_widget(Paragraph::new("─".repeat(w as usize)).style(th.muted()), parts[4]);
    f.render_widget(Paragraph::new("─".repeat(w as usize)).style(th.muted()), parts[6]);
    let note = if is_host && g.auto_update { "host_auto ON: timer updates host first, CTs if OK; never reboots" } else if is_host { "host_auto OFF: timer skips the host; press a to include it" } else { "config is read from the .conf file (no processes); runtime metrics come from cgroup/pidfile only" };
    f.render_widget(Paragraph::new(keys.into_iter().chain([Line::styled(trunc(note, w as usize), th.muted())]).collect::<Vec<_>>()), parts[7]);
}

// ---------------------------------------------------------------- ayuda

type HelpSection = (&'static str, Vec<(&'static str, &'static str)>);

fn help_sections(app: &App) -> Vec<HelpSection> {
    let keys = |t: &'static str, r: Vec<(&'static str, &'static str)>| -> Vec<HelpSection> { vec![(t, r)] };
    match app.view {
        View::List => vec![
            ("Keys", vec![
                ("↑↓ / j k", "move cursor"),
                ("PgUp PgDn", "move 10 · Home End first/last"),
                ("1 2 3 4", "filter All / Running / Stopped / Updates"),
                ("/", "search name, tag or id (Esc clears)"),
                ("s", "cycle sort: vmid · name · updates · ram"),
                ("r", "reload /etc/pve confs (files only)"),
                ("Enter", "guest detail"),
                ("c", "check LXC updates (visible CTs, or all): refreshes the package indexes"),
                ("C", "read-only check: does not refresh the indexes"),
                ("v", "show the last check result"),
                ("o", "choose visible columns (Space toggles; saved in config.toml)"),
                ("u", "update the visible CTs with updates (asks first): temporary backup, verify, auto-rollback on failure"),
                ("a", "toggle scheduled auto-update of the CT (asks first); host row: host_auto · host/GPU CT: also the NVIDIA driver checkbox (Space toggles, Enter saves)"),
                ("b", "toggle the backup before updating the CT (asks first)"),
                ("g / S", "config view + timer state · S change schedule (asks first)"),
                ("l", "run logs (one file per update run)"),
                ("?", "this help · q quit"),
            ]),
            ("Columns", vec![
                ("UPD", "updates available, all/security"),
                ("—", "no data (run c)"),
                ("AUTO", "yes = scheduled auto-update"),
                ("BACKUP", "backup before updating (o chooses columns)"),
            ]),
        ],
        View::Logs if app.logs.viewer.is_some() => keys("Keys", vec![("↑↓ / j k", "scroll · PgUp PgDn / Space page · mouse wheel"), ("g / G", "top / end"), ("n / p", "next (older) / previous (newer) log"), ("q / Esc", "back to the list of logs"), ("?", "this help")]),
        View::Logs => keys("Keys", vec![("↑↓ / j k", "select · PgUp PgDn 10 · g / G first / last"), ("Enter", "open the log (or double click)"), ("Esc / l", "back to the main screen"), ("?", "this help · q quit")]),
        View::Storage if app.storage.detail.is_some() => keys("Keys", vec![("↑↓ / j k", "select a backup (the page follows) · with no backups, scroll"), ("x", "delete the selected backup (asks first; runs `pvesm free <volume>`; protected backups cannot be deleted)"), ("PgUp PgDn", "scroll a page · g / G top / end"), ("r", "refresh"), ("Esc / q", "back to the guests"), ("?", "this help")]),
        View::Storage => vec![
            ("Keys", vec![
                ("↑↓ / j k", "select · PgUp PgDn page · g / G first / last"),
                ("Enter / d", "space detail: disks, snapshots, backups (x deletes one), pre-update mode"),
                ("S", "cycle sort: used · alloc · % · last backup (oldest first) · id"),
                ("f", "show only guests with alerts"),
                ("r", "refresh (reads again; the screen keeps working)"),
                ("Esc / q", "back to the main screen"),
                ("?", "this help"),
            ]),
            ("Columns", vec![
                ("USED / %", "what Proxmox shows (disk / maxdisk of the guest), not the thin-pool blocks"),
                ("—", "stopped guest or not readable"),
                ("* in SNAPS", "has pxu-* snapshots"),
                ("LAST BACKUP", "ignores pxu pre-update leftovers"),
                ("WHERE", "storage that holds that last backup (wide screens only)"),
                ("Dedup (PBS)", "logical size of the backups / space used in the datastore"),
            ]),
            ("Alerts", vec![
                ("no backup", "the guest has no backup"),
                ("last backup > 30 d", "the newest backup is older than 30 days"),
                ("disk >= 98%", "the guest disk is nearly full"),
                ("thin pool", "overcommitted or >= 90% full"),
                ("backup target", "pxu backup target without room for the biggest CT (like the pre-vzdump check)"),
            ]),
            ("Detail", vec![("thin-pool blocks", "suggests `pct fstrim <id>` when they exceed the real use by > 15 points (pxu never runs it)")]),
        ],
        View::Config => keys("Keys", vec![("s", "change the update schedule (asks first; server time)"), ("d", "change log_retention_days (0 = never delete)"), ("l", "run logs"), ("n", "edit [apprise] (api_url, key or urls, tag, events; secrets masked; asks first)"), ("t", "send a test notice (like `pxu notify-test`)"), ("r", "re-read the timer state"), ("Esc", "back to the list"), ("?", "this help · q quit")]),
        View::Update => keys("Keys", vec![("x / Esc", "cancel: finish the CT in progress, then stop"), ("↑↓ PgUp PgDn", "scroll the log · f / End follow · Home oldest"), ("Esc", "back to the list (when the run is over)"), ("?", "this help")]),
        View::Check => keys("Keys", vec![("↑↓ / j k", "select CT (packages shown below)"), ("u", "update the CTs with updates (asks first)"), ("c", "re-check (refreshes the indexes) · C / R read-only check"), ("Enter", "guest detail"), ("Esc", "cancel a running check, then back"), ("?", "this help · q quit")]),
        View::Detail(_) => keys("Keys", vec![
            ("u", "update this CT (asks first)"),
            ("a", "toggle this CT's scheduled auto-update (asks first); host: host_auto · host/GPU CT: also the NVIDIA driver checkbox (Space toggles, Enter saves)"),
            ("b", "toggle this CT's backup before updating (asks first)"),
            ("g / S", "config view · change the update schedule"),
            ("↑↓ / j k", "scroll"),
            ("PgUp PgDn", "scroll 10 · Home top"),
            ("r", "reload confs"),
            ("Esc", "back to the list"),
            ("?", "this help · q quit"),
        ]),
    }
}

/// Ayuda como tabla de dos columnas (clave | descripción). La descripción se envuelve bajo su columna.
fn help_lines(sections: &[HelpSection], w: usize, th: &Theme, blank_between: bool) -> Vec<Line<'static>> {
    let kw = sections.iter().flat_map(|(_, r)| r.iter()).map(|(k, _)| k.chars().count()).max().unwrap_or(0) + 2;
    let dw = w.saturating_sub(kw + 1).max(8);
    let mut out: Vec<Line<'static>> = Vec::new();
    for (i, (title, rows)) in sections.iter().enumerate() {
        if i > 0 && blank_between {
            out.push(Line::default());
        }
        out.push(rule_spans(&[(title, th.accent().add_modifier(Modifier::BOLD))], w, th));
        for (k, d) in rows {
            let wrapped = wrap_words(d, dw);
            for (j, part) in wrapped.into_iter().enumerate() {
                let key = if j == 0 { format!(" {k:<kw$}") } else { " ".repeat(kw + 1) };
                out.push(Line::from(vec![Span::styled(key, th.accent()), Span::raw(part)]));
            }
        }
    }
    out
}

/// Parte `s` en filas de hasta `w` columnas, solo en espacios (una palabra más larga que `w` se parte).
fn wrap_words(s: &str, w: usize) -> Vec<String> {
    let mut rows: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in s.split(' ') {
        let mut word = word.to_string();
        loop {
            let (cl, wl) = (cur.chars().count(), word.chars().count());
            if cur.is_empty() && wl <= w {
                cur = word;
                break;
            } else if !cur.is_empty() && cl + 1 + wl <= w {
                cur.push(' ');
                cur.push_str(&word);
                break;
            } else if !cur.is_empty() {
                rows.push(std::mem::take(&mut cur));
            } else {
                rows.push(word.chars().take(w).collect());
                word = word.chars().skip(w).collect();
            }
        }
    }
    if !cur.is_empty() || rows.is_empty() {
        rows.push(cur);
    }
    rows
}

fn draw_help(f: &mut Frame, app: &App, th: &Theme, area: Rect) {
    let sections = help_sections(app);
    let w = 100.min(area.width.saturating_sub(4));
    let inner_w = (w as usize).saturating_sub(2);
    let foot = if app.read_only { "read-only: no command that writes can run" } else { "writes only after you confirm (an update run, an auto-update change or deleting a backup)" };
    let max_h = area.height as usize;
    // Marco 2 + pie 2 (línea en blanco + texto); si no cabe con separadores, sin ellos; si aún no, scroll.
    let mut body = help_lines(&sections, inner_w, th, true);
    if body.len() + 4 > max_h {
        body = help_lines(&sections, inner_w, th, false);
    }
    let room = max_h.saturating_sub(4);
    let max_scroll = body.len().saturating_sub(room);
    let scroll = app.help_scroll.get().min(max_scroll);
    app.help_scroll.set(scroll);
    let shown = body.len().min(room);
    let h = (shown + 4) as u16;
    let r = Rect { x: (area.width - w) / 2, y: (area.height.saturating_sub(h)) / 2, width: w, height: h.min(area.height) };
    f.render_widget(Clear, r);
    let title = if max_scroll > 0 { " help · ↑↓ scroll · any other key closes " } else { " help · any key closes " };
    let blk = Block::default().borders(Borders::ALL).title(Span::styled(title, th.bold())).title_bottom(Line::styled(concat!(" pxu ", env!("CARGO_PKG_VERSION"), " "), th.muted()).alignment(Alignment::Right)).border_style(th.accent());
    let inner = blk.inner(r);
    f.render_widget(blk, r);
    let mut lines: Vec<Line> = body.into_iter().skip(scroll).take(shown).collect();
    lines.push(Line::default());
    lines.push(Line::styled(foot, th.muted()).alignment(Alignment::Left));
    f.render_widget(Paragraph::new(lines), inner);
}

// ---------------------------------------------------------------- chequeo (DESIGN §5.3)

fn draw_check(f: &mut Frame, app: &App, th: &Theme, area: Rect) {
    let w = area.width;
    let wide = w >= 112;
    let ck = &app.check;
    let pk_h: u16 = if area.height >= 40 { 10 } else { 7 };
    let keys = if wide {
        keybar_wrap(th, &[("↑↓/jk", "select"), ("Enter", "detail"), ("c", "refresh+check"), ("C", "read-only check"), ("u", "update (asks)"), ("Esc", "back/cancel"), ("?", "help"), ("q", "quit")], 2, w as usize, 2)
    } else {
        keybar_wrap(th, &[("↑↓", "select"), ("Enter", "detail"), ("c", "refresh+check"), ("C", "read-only"), ("Esc", "back"), ("?", "help"), ("q", "quit")], 2, w as usize, 2)
    };
    let parts = Layout::vertical([
        Constraint::Length(1), // header
        Constraint::Length(1), // resumen
        Constraint::Length(1), // encabezado de columnas
        Constraint::Min(1),    // filas
        Constraint::Length(pk_h),
        Constraint::Length(1), // regla
        Constraint::Length(1), // estado
        Constraint::Length(1), // regla
        Constraint::Length(keys.len() as u16), // teclas
    ])
    .split(area);

    let mode = if ck.refresh { "REFRESH" } else { "read-only" };
    let right = if ck.running {
        format!("{mode} · {}/{} checked{}   {}", ck.results.len(), ck.total, if ck.cancelling { " · cancelling" } else { "" }, app.clock)
    } else {
        format!("{mode} · finished {} · took {} s   {}", ck.finished, ck.took_s, app.clock)
    };
    f.render_widget(Paragraph::new(two_sided(vec![Span::styled("pxu · check", th.bold()), Span::styled(badges(app), th.muted())], right, w)), parts[0]);

    let sum = match &ck.report {
        Some(r) => summary_line(r),
        None => "running…".into(),
    };
    f.render_widget(Paragraph::new(trunc(&sum, w as usize)).style(th.warn()), parts[1]);

    let nw = if wide { 20 } else { 14 };
    let head = format!("  {:>5}  {:<nw$}  {:<6}  {:>5}  {:>3}  {:>3}  {:<6}  RESULT", "ID", "NAME", "PM", "INDEX", "UPD", "SEC", "REBOOT");
    f.render_widget(Paragraph::new(trunc(&head, w as usize)).style(th.muted()), parts[2]);

    let rows_h = parts[3].height as usize;
    let n = ck.results.len();
    let top = if ck.cursor >= rows_h { ck.cursor + 1 - rows_h } else { 0 };
    let mut lines = Vec::new();
    for (i, r) in ck.results.iter().enumerate().skip(top).take(rows_h) {
        let inf = r.info.as_ref();
        let txt = format!(
            "{:>5}  {:<nw$}  {:<6}  {:>5}  {:>3}  {:>3}  {:<6}  {}",
            r.vmid,
            trunc(&r.name, nw),
            inf.map_or("—", |i| i.pm.as_str()),
            inf.map_or("—".into(), |i| format!("{}{}", index_label(i.index_age_s, i.index_age_s > 0), if r.stale { "*" } else { "" })),
            inf.map_or("—".into(), |_| r.updates().to_string()),
            inf.map_or("—".into(), |_| r.security().to_string()),
            inf.map_or("—", |i| if i.reboot_required { "yes" } else { "no" }),
            status_label(r),
        );
        let sel = i == ck.cursor;
        let txt = format!("{}{}", if sel { "> " } else { "  " }, txt);
        let style = if sel {
            th.sel()
        } else if matches!(r.status, Status::Error | Status::Timeout) {
            th.warn()
        } else if r.updates() > 0 {
            th.ok()
        } else if r.info.is_none() {
            th.muted()
        } else {
            Style::default()
        };
        lines.push(Line::styled(trunc(&txt, w as usize), style));
    }
    if n == 0 {
        lines.push(Line::styled("  waiting for the first CT…", th.muted()));
    }
    f.render_widget(Paragraph::new(lines), parts[3]);

    // Paquetes del CT bajo el cursor.
    let blk = Block::default().borders(Borders::TOP).title(match ck.results.get(ck.cursor) {
        Some(r) => format!(" packages · {} {} · {} ", r.vmid, r.name, r.info.as_ref().map_or("—", |i| i.pm.as_str())),
        None => " packages ".into(),
    });
    let inner = blk.inner(parts[4]);
    f.render_widget(blk.border_style(th.accent()), parts[4]);
    let mut pl = Vec::new();
    if let Some(r) = ck.results.get(ck.cursor) {
        match &r.info {
            Some(i) if !i.packages.is_empty() => {
                let room = (inner.height as usize).saturating_sub(1);
                let shown = if i.packages.len() > room { room.saturating_sub(1) } else { i.packages.len() };
                for p in i.packages.iter().take(shown) {
                    let t = format!("{:<24}  {:<16}  → {:<16}  {}", trunc(&p.name, 24), trunc(&p.cur, 16), trunc(&p.new, 16), if p.sec { "sec" } else { "" });
                    pl.push(Line::styled(trunc(&t, inner.width as usize), if p.sec { th.warn() } else { Style::default() }));
                }
                if i.packages.len() > shown {
                    pl.push(Line::styled(format!("+ {} more", i.packages.len() - shown), th.muted()));
                }
                let held = if i.held.is_empty() { "none".to_string() } else { i.held.join(", ") };
                pl.push(Line::styled(trunc(&format!("held: {held} · reboot required: {}", if i.reboot_required { "yes" } else { "no" }), inner.width as usize), th.muted()));
            }
            Some(_) => pl.push(Line::styled("up to date", th.ok())),
            None => pl.push(Line::styled(trunc(&status_label(r), inner.width as usize), th.muted())),
        }
    }
    f.render_widget(Paragraph::new(pl), inner);

    let st = if !app.status.is_empty() {
        app.status.clone()
    } else if ck.refresh {
        "refresh ON: indexes were updated inside the CTs".to_string()
    } else {
        "read-only: 1 command per CT, nothing written in the guests · INDEX = age of the guest's own index (* stale)".to_string()
    };
    f.render_widget(Paragraph::new(trunc(&st, w as usize)).style(th.accent()), parts[6]);
    f.render_widget(Paragraph::new("─".repeat(w as usize)).style(th.muted()), parts[5]);
    f.render_widget(Paragraph::new("─".repeat(w as usize)).style(th.muted()), parts[7]);
    f.render_widget(Paragraph::new(keys), parts[8]);
}

// ---------------------------------------------------------------- update (DESIGN §5.4, §5.5, §7)

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width.saturating_sub(2));
    let h = h.min(area.height);
    Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h }
}

fn draw_host_modal(f: &mut Frame, app: &App, th: &Theme, area: Rect) {
    use pxu::share::update::hostupdate as hu;
    let Some(m) = &app.host_modal else { return };
    let rows = if m.dpkg.is_some() { 10 } else { hu::grouped(&m.pkgs).iter().map(|(_, v)| v.len() + 1).sum::<usize>() + 8 + m.note.is_some() as usize + m.nvidia.is_some() as usize + hu::has_kernel(&m.pkgs) as usize };
    let h = (rows as u16).min(area.height.saturating_sub(2));
    let r = centered(area, 98, h);
    f.render_widget(Clear, r);
    let title = if m.dpkg.is_some() { " host: dpkg needs repair " } else { " confirm host update " };
    let blk = Block::default().borders(Borders::ALL).title(Span::styled(title, th.bold())).border_style(th.accent());
    let inner = blk.inner(r);
    f.render_widget(blk, r);
    let w = inner.width as usize;
    let mut lines: Vec<Line> = Vec::new();
    if let Some(d) = &m.dpkg {
        lines.push(Line::styled(" `dpkg --audit` reports a half-configured system:", th.bold()));
        for l in d.iter().take(8) {
            lines.push(Line::styled(trunc(&format!("   {l}"), w), th.warn()));
        }
        lines.push(Line::default());
        lines.push(Line::raw(" Offer: run `dpkg --configure -a`, then review the update again."));
        lines.push(Line::default());
        lines.push(mbar(th, &[("Enter", "run dpkg --configure -a"), ("Esc", "cancel")], 2, w));
        f.render_widget(Paragraph::new(lines), inner);
        return;
    }
    let n = m.pkgs.len();
    let sec = m.pkgs.iter().filter(|p| p.sec).count();
    lines.push(Line::styled(format!(" Install {n} package{} on the Proxmox host ({sec} security)?", if n == 1 { "" } else { "s" }), th.bold()));
    if let Some(note) = &m.note {
        lines.push(Line::styled(trunc(&format!(" ! {note}"), w), th.warn()));
    }
    lines.push(Line::default());
    let mut body: Vec<Line> = Vec::new();
    for (g, v) in hu::grouped(&m.pkgs) {
        body.push(Line::styled(format!(" {g} ({})", v.len()), th.accent()));
        for p in v {
            body.push(Line::raw(trunc(&format!("   {} {} → {}", p.name, p.cur, p.new), w)));
        }
    }
    let mut foot: Vec<Line> = Vec::new();
    if hu::has_kernel(&m.pkgs) {
        foot.push(Line::styled(" ! a new kernel: a reboot will be needed (pxu never reboots)", th.warn()));
    }
    if let Some(k) = &m.nvidia {
        foot.push(Line::styled(trunc(&format!(" i NVIDIA module missing for kernel {k} (informational)"), w), th.muted()));
    }
    foot.push(Line::styled(" /etc is backed up first · no automatic rollback · log in /var/log/pxu", th.muted()));
    let room = (inner.height as usize).saturating_sub(lines.len() + foot.len() + 2);
    let top = m.scroll.min(body.len().saturating_sub(room));
    lines.extend(body.into_iter().skip(top).take(room));
    lines.push(Line::default());
    lines.extend(foot);
    lines.push(mbar(th, &[("Enter", "install"), ("↑↓/jk", "scroll"), ("Esc", "cancel")], 2, w));
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_modal(f: &mut Frame, app: &App, th: &Theme, area: Rect) {
    let Some(m) = &app.modal else { return };
    let chosen = m.chosen().len();
    let h = (m.cands.len() as u16 + 12).min(area.height.saturating_sub(2));
    let r = centered(area, 98, h);
    f.render_widget(Clear, r);
    let blk = Block::default().borders(Borders::ALL).title(Span::styled(" confirm ", th.bold())).border_style(th.accent());
    let inner = blk.inner(r);
    f.render_widget(blk, r);
    let w = inner.width as usize;
    let mut lines = vec![Line::styled(format!(" Update {chosen} guest{}?", if chosen == 1 { "" } else { "s" }), th.bold()), Line::default()];
    lines.extend(m.head.iter().map(|l| Line::raw(trunc(&format!(" {l}"), w))));
    lines.push(Line::default());
    let room = (inner.height as usize).saturating_sub(lines.len() + 5);
    let top = if m.cursor >= room { m.cursor + 1 - room } else { 0 };
    for (i, c) in m.cands.iter().enumerate().skip(top).take(room) {
        let mark = match (&c.skip, m.excluded.contains(&c.vmid)) {
            (Some(_), _) => "·",
            (None, true) => " ",
            (None, false) => "✓",
        };
        let what = match &c.skip {
            Some(why) => format!("skip: {why}"),
            None => format!("{} pkg{} ({} sec)", c.packages, if c.packages == 1 { "" } else { "s" }, c.security),
        };
        let t = format!("{}{} {:>5} {:<18} {}", if i == m.cursor { ">" } else { " " }, mark, c.vmid, trunc(&c.name, 18), what);
        let style = if i == m.cursor { th.sel() } else if c.skip.is_some() { th.muted() } else { Style::default() };
        lines.push(Line::styled(trunc(&t, w), style));
    }
    lines.push(Line::default());
    lines.extend(m.foot.iter().map(|l| Line::styled(trunc(&format!(" {l}"), w), th.warn())));
    lines.push(mbar(th, &[("Space", "include/exclude"), ("Enter", "start"), ("Esc", "cancel")], 3, w));
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_col_modal(f: &mut Frame, app: &App, th: &Theme, area: Rect) {
    let Some((cur, _)) = app.col_modal else { return };
    let names = pxu::share::config::OPTIONAL_COLUMNS;
    let r = centered(area, 46, (names.len() as u16 + 6).min(area.height.saturating_sub(2)));
    f.render_widget(Clear, r);
    let blk = Block::default().borders(Borders::ALL).title(Span::styled(" columns ", th.bold())).border_style(th.accent());
    let inner = blk.inner(r);
    f.render_widget(blk, r);
    let w = inner.width as usize;
    let mut lines = vec![Line::styled(" ID, NAME and STATE are always shown", th.muted()), Line::default()];
    for (i, n) in names.iter().enumerate() {
        let t = format!("{} [{}] {}", if i == cur { ">" } else { " " }, if app.col_shown(n) { "x" } else { " " }, n.to_uppercase());
        lines.push(Line::styled(trunc(&t, w), if i == cur { th.sel() } else { Style::default() }));
    }
    lines.push(Line::default());
    lines.push(mbar(th, &[("Space", "show/hide"), ("Enter", "close")], 2, w));
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_auto_modal(f: &mut Frame, app: &App, th: &Theme, area: Rect) {
    let Some(m) = &app.auto_modal else { return };
    let info = m.lines();
    let ct = m.vmid != 0;
    let drv = m.driver.as_ref().map(|d| d.lines(ct)).unwrap_or_default();
    let h = info.len() + drv.len() + 12;
    let r = centered(area, 100, (h as u16).min(area.height.saturating_sub(2)));
    f.render_widget(Clear, r);
    let blk = Block::default().borders(Borders::ALL).title(Span::styled(" auto-update ", th.bold())).border_style(th.accent());
    let inner = blk.inner(r);
    f.render_widget(blk, r);
    let w = inner.width as usize;
    let what = if m.vmid == 0 { "the Proxmox HOST".to_string() } else { format!("CT {} {}", m.vmid, m.name) };
    let mut lines = vec![Line::styled(trunc(&format!(" Automatic update options of {what}"), w), th.bold()), Line::default()];
    lines.extend(info.iter().map(|l| Line::raw(trunc(&format!(" {l}"), w))));
    lines.push(Line::default());
    let boxed = |on: bool| if on { "x" } else { " " };
    let mark = |i: usize| if m.focus == i { ">" } else { " " };
    lines.push(Line::styled(trunc(&format!("{} [{}] Automatic updates", mark(0), boxed(m.value)), w), if m.focus == 0 { th.sel() } else { th.bold() }));
    match &m.driver {
        Ok(d) => {
            let style = if !m.value { th.muted() } else if m.focus == 1 { th.sel() } else { th.bold() };
            lines.push(Line::styled(trunc(&format!("{} [{}] Include GPU driver update (NVIDIA){}", mark(1), boxed(d.checked), if m.value { "" } else { "  — needs automatic updates" }), w), style));
            lines.extend(drv.iter().map(|l| Line::styled(trunc(&format!("      {l}"), w), th.muted())));
        }
        Err(why) => lines.push(Line::styled(trunc(&format!("  GPU driver update: not offered — {why}"), w), th.muted())),
    }
    lines.push(Line::default());
    lines.push(Line::raw(trunc(&format!(" {}", m.saves()), w)));
    if let Some(wr) = m.warning() {
        lines.push(Line::styled(trunc(&format!(" {wr}"), w), th.warn()));
    }
    lines.push(Line::default());
    lines.push(mbar(th, &[("↑↓", "move"), ("Space", "check"), ("Enter", "save"), ("Esc", "cancel")], 2, w));
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_backup_modal(f: &mut Frame, app: &App, th: &Theme, area: Rect) {
    let Some(m) = &app.backup_modal else { return };
    let r = centered(area, 90, (m.lines.len() as u16 + 6).min(area.height.saturating_sub(2)));
    f.render_widget(Clear, r);
    let blk = Block::default().borders(Borders::ALL).title(Span::styled(" confirm ", th.bold())).border_style(th.accent());
    let inner = blk.inner(r);
    f.render_widget(blk, r);
    let w = inner.width as usize;
    let head = format!(" {} the backup before updating CT {} {}?", if m.disabled { "Disable" } else { "Enable" }, m.vmid, m.name);
    let mut lines = vec![Line::styled(trunc(&head, w), th.bold()), Line::default()];
    lines.extend(m.lines.iter().map(|l| Line::styled(trunc(&format!(" {l}"), w), if l.starts_with("WARNING") { th.warn() } else { Style::default() })));
    lines.push(Line::default());
    lines.push(mbar(th, &[("y/Enter", "save"), ("Esc", "cancel")], 2, w));
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_backup_del_modal(f: &mut Frame, app: &App, th: &Theme, area: Rect) {
    let Some(m) = &app.backup_del else { return };
    let r = centered(area, 100, (m.lines.len() as u16 + 6).min(area.height.saturating_sub(2)));
    f.render_widget(Clear, r);
    let blk = Block::default().borders(Borders::ALL).title(Span::styled(" confirm ", th.bold())).border_style(th.accent());
    let inner = blk.inner(r);
    f.render_widget(blk, r);
    let w = inner.width as usize;
    let mut lines = vec![Line::styled(trunc(&format!(" Delete this backup of CT {}?", m.vmid), w), th.bold()), Line::default()];
    lines.extend(m.lines.iter().map(|l| Line::styled(trunc(&format!(" {l}"), w), if l.starts_with("WARNING") { th.warn() } else { Style::default() })));
    lines.push(Line::default());
    lines.push(mbar(th, &[("y/Enter", "delete"), ("Esc/n", "cancel")], 2, w));
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_config(f: &mut Frame, app: &App, th: &Theme, area: Rect) {
    let w = area.width;
    let keys = config_keys(th, w);
    let parts = Layout::vertical([Constraint::Length(1), Constraint::Length(1), Constraint::Length(1), Constraint::Min(3), Constraint::Length(1), Constraint::Length(1), Constraint::Length(1), Constraint::Length(keys.len() as u16 + 1)]).split(area);
    f.render_widget(Paragraph::new(two_sided(vec![Span::styled("pxu · config".to_string(), th.bold()), Span::styled(badges(app), th.warn())], app.clock.clone(), w)), parts[0]);
    f.render_widget(Paragraph::new(trunc("config · automatic update schedule (systemd timer pxu-update) and notifications (Apprise)", w as usize)).style(th.muted()), parts[1]);
    f.render_widget(Paragraph::new("─".repeat(w as usize)).style(th.muted()), parts[2]);
    let yn = |b: bool| if b { "yes" } else { "no" };
    let mut rows: Vec<String> = Vec::new();
    match &app.timer {
        None => rows.push("timer state not read yet (press r)".into()),
        Some(Err(e)) => rows.push(format!("cannot read the timer state: {e}")),
        Some(Ok(t)) => {
            rows.push(format!("units installed : {}", yn(t.installed)));
            rows.push(format!("enabled         : {}{}", yn(t.enabled), if t.installed { format!(" (state: {})", t.active) } else { String::new() }));
            rows.push(format!("schedule        : {}", if t.calendars.is_empty() { "—".to_string() } else { t.calendars.join(" ; ") }));
            rows.push(format!("next run        : {}", t.next.as_deref().unwrap_or("—")));
            rows.push("times are the server's".into());
        }
    }
    let auto = app.inv.guests.iter().filter(|g| g.kind == GuestKind::Lxc && g.auto_update).count();
    rows.push(String::new());
    rows.push(format!("config          : {}{}", pxu::share::config::CONFIG_PATH, if app.read_only { " (missing or not usable: read-only)" } else { "" }));
    let host_auto = app.inv.guests.iter().any(|g| g.kind == GuestKind::Host && g.auto_update);
    rows.push(format!("CTs with auto-update: {auto}{}", if auto == 0 && !host_auto { " — the scheduled update would do nothing" } else { "" }));
    rows.push(format!("host_auto       : {}", if host_auto { "yes — the host is updated FIRST, the CTs only if it ends well (never rebooted)" } else { "no — the scheduled run never touches the host" }));
    if let Some(r) = &app.ro_reason {
        rows.push(format!("! {r}"));
    }
    rows.push(format!("log retention   : {}", match app.log_days { None => "—".to_string(), Some(0) => "never delete run logs (log_retention_days = 0)".to_string(), Some(d) => format!("{d} days (run logs in /var/log/pxu older than this are deleted after each run)") }));
    rows.push(String::new());
    rows.push("Notifications (Apprise)".into());
    match &app.apprise {
        None => rows.push("not read yet (press r)".into()),
        Some(Err(e)) => rows.push(format!("cannot read {}: {e}", pxu::share::config::CONFIG_PATH)),
        Some(Ok(v)) => rows.extend(v.iter().cloned()),
    }
    let blk = Block::default().borders(Borders::ALL).title(Span::styled(" Schedule ", th.bold())).border_style(th.muted());
    let inner = blk.inner(parts[3]);
    f.render_widget(blk, parts[3]);
    f.render_widget(Paragraph::new(rows.iter().map(|r| Line::raw(trunc(&format!(" {r}"), inner.width as usize))).collect::<Vec<_>>()), inner);
    let hint = if !app.status.is_empty() { app.status.clone() } else { "[s] schedule  [n] notifications  [d] log days  [t] test notice".into() };
    f.render_widget(Paragraph::new(trunc(&hint, w as usize)).style(th.accent()), parts[5]);
    f.render_widget(Paragraph::new("─".repeat(w as usize)).style(th.muted()), parts[4]);
    f.render_widget(Paragraph::new("─".repeat(w as usize)).style(th.muted()), parts[6]);
    f.render_widget(Paragraph::new(keys.into_iter().chain([Line::styled(trunc("the timer runs `pxu check` then `pxu update --yes` (host first if host_auto; then the CTs with auto-update)", w as usize), th.muted())]).collect::<Vec<_>>()), parts[7]);
}

fn log_result_style(th: &Theme, r: &str) -> Style {
    if r.starts_with('✅') {
        th.ok()
    } else if r.starts_with('⚠') {
        th.warn()
    } else if r.starts_with('❌') || r.starts_with('⛔') {
        th.fg(Color::Red)
    } else {
        Style::default()
    }
}

/// Página Logs: lista (Date / Duration / Result / Size) o visor de un log.
fn draw_logs(f: &mut Frame, app: &App, th: &Theme, area: Rect) {
    use pxu::share::update::report::dur;
    use pxu::share::update::runlog::{local_ymd_hm, size_text};
    let w = area.width;
    let lg = &app.logs;
    let keys = if lg.viewer.is_some() {
        keybar_wrap(th, &[("↑↓/PgUp/PgDn", "scroll"), ("g/G", "top/end"), ("n/p", "next/prev"), ("q/Esc", "back"), ("?", "help")], 2, w as usize, 2)
    } else {
        keybar_wrap(th, &[("↑↓/jk", "select"), ("Enter", "open"), ("Esc", "back"), ("?", "help"), ("q", "quit")], 2, w as usize, 2)
    };
    let parts = Layout::vertical([Constraint::Length(1), Constraint::Length(1), Constraint::Length(1), Constraint::Min(1), Constraint::Length(1), Constraint::Length(1), Constraint::Length(1), Constraint::Length(keys.len() as u16)]).split(area);
    let body = parts[3];
    let status = if app.status.is_empty() { String::new() } else { app.status.clone() };
    if let Some(v) = &lg.viewer {
        let meta = lg.entries.get(lg.sel).and_then(|e| e.meta.as_ref());
        let head = match meta {
            Some(m) if m.complete => format!("{} · {} · {}", v.title, m.result, dur(m.duration)),
            Some(m) => format!("{} · {}", v.title, m.result),
            None => v.title.clone(),
        };
        f.render_widget(Paragraph::new(two_sided(vec![Span::styled("pxu · log".to_string(), th.bold()), Span::styled(badges(app), th.warn())], app.clock.clone(), w)), parts[0]);
        f.render_widget(Paragraph::new(trunc(&head, w as usize)).style(meta.map_or(th.muted(), |m| log_result_style(th, &m.result))), parts[1]);
        f.render_widget(Paragraph::new("─".repeat(w as usize)).style(th.muted()), parts[2]);
        let h = body.height as usize;
        lg.body_h.set(h);
        let top = v.top.min(v.lines.len().saturating_sub(h));
        let lines: Vec<Line> = v.lines.iter().skip(top).take(h).map(|l| Line::raw(trunc(l, w as usize))).collect();
        f.render_widget(Paragraph::new(lines), body);
        let end = (top + h).min(v.lines.len());
        let ind = format!("lines {}-{} of {}", if v.lines.is_empty() { 0 } else { top + 1 }, end, v.lines.len());
        f.render_widget(Paragraph::new(trunc(&if status.is_empty() { ind } else { status }, w as usize)).style(th.accent()), parts[5]);
        f.render_widget(Paragraph::new("─".repeat(w as usize)).style(th.muted()), parts[4]);
        f.render_widget(Paragraph::new("─".repeat(w as usize)).style(th.muted()), parts[6]);
        f.render_widget(Paragraph::new(keys), parts[7]);
        return;
    }
    f.render_widget(Paragraph::new(two_sided(vec![Span::styled("pxu · logs".to_string(), th.bold()), Span::styled(badges(app), th.warn())], app.clock.clone(), w)), parts[0]);
    f.render_widget(Paragraph::new(trunc(&format!("update run logs · newest first · {} in {}", lg.entries.len(), pxu::share::update::LOG_DIR), w as usize)).style(th.muted()), parts[1]);
    let hc = Layout::horizontal([Constraint::Length(18), Constraint::Length(10), Constraint::Min(10), Constraint::Length(9)]).split(parts[2]);
    for (r, t) in hc.iter().zip(["  Date", "Duration", "Result", "Size"]) {
        f.render_widget(Paragraph::new(t).style(th.bold()).alignment(if t == "Size" { Alignment::Right } else { Alignment::Left }), *r);
    }
    let h = body.height as usize;
    lg.rows_h.set(h);
    lg.rows_y.set(body.y);
    if lg.entries.is_empty() {
        lg.off.set(0);
        f.render_widget(Paragraph::new(" no run logs yet").style(th.muted()), body);
    } else {
        let sel = lg.sel.min(lg.entries.len() - 1);
        let mut off = lg.off.get().min(lg.entries.len().saturating_sub(1));
        if sel < off {
            off = sel;
        } else if sel >= off + h {
            off = sel + 1 - h;
        }
        lg.off.set(off);
        // una celda por columna: ratatui cuenta bien el ancho del emoji del resultado
        for (n, (i, e)) in lg.entries.iter().enumerate().skip(off).take(h).enumerate() {
            let row = Rect { x: body.x, y: body.y + n as u16, width: body.width, height: 1 };
            let (d, r) = match &e.meta {
                Some(m) if m.complete => (dur(m.duration), m.result.clone()),
                Some(m) => ("—".to_string(), m.result.clone()),
                None => ("—".to_string(), "unreadable header".to_string()),
            };
            let st = if i == sel { th.sel() } else { e.meta.as_ref().map_or(th.muted(), |m| log_result_style(th, &m.result)) };
            f.render_widget(Paragraph::new("").style(st), row);
            let c = Layout::horizontal([Constraint::Length(18), Constraint::Length(10), Constraint::Min(10), Constraint::Length(9)]).split(row);
            f.render_widget(Paragraph::new(format!(" {}", local_ymd_hm(e.started))).style(st), c[0]);
            f.render_widget(Paragraph::new(d).style(st), c[1]);
            f.render_widget(Paragraph::new(trunc(&r, c[2].width as usize)).style(st), c[2]);
            f.render_widget(Paragraph::new(size_text(e.size)).style(st).alignment(Alignment::Right), c[3]);
        }
    }
    let ind = if lg.entries.is_empty() { String::new() } else { format!("{} / {}", lg.sel.min(lg.entries.len() - 1) + 1, lg.entries.len()) };
    f.render_widget(Paragraph::new(trunc(&if status.is_empty() { ind } else { status }, w as usize)).style(th.accent()), parts[5]);
    f.render_widget(Paragraph::new("─".repeat(w as usize)).style(th.muted()), parts[4]);
    f.render_widget(Paragraph::new("─".repeat(w as usize)).style(th.muted()), parts[6]);
    f.render_widget(Paragraph::new(keys), parts[7]);
}

// ---------------------------------------------------------------- storage (DESIGN §5.7)

fn sz(b: Option<u64>) -> String {
    b.map_or("—".into(), pxu::share::storage::fmt_gb)
}

fn pct_text(p: Option<u64>) -> String {
    p.map_or("—".into(), |p| format!("{p}%"))
}

fn use_style(th: &Theme, p: Option<u64>) -> Style {
    match p {
        Some(p) if p >= 90 => th.fg(Color::LightRed),
        Some(p) if p >= 70 => th.warn(),
        Some(_) => th.ok(),
        None => th.muted(),
    }
}

/// `[████░░░░]` de `w` celdas.
fn bar(p: Option<u64>, w: usize) -> String {
    let full = (p.unwrap_or(0).min(100) as usize * w).div_ceil(100).min(w);
    format!("[{}{}]", "█".repeat(full), "░".repeat(w - full))
}

fn left(s: &str, w: usize) -> String {
    format!("{:<w$}", trunc(s, w))
}

fn right(s: &str, w: usize) -> String {
    format!("{:>w$}", trunc(s, w))
}

fn alert_text(a: &pxu::share::storage::Alert) -> String {
    use pxu::share::storage::Alert;
    match a {
        Alert::NoBackup => "no backup".into(),
        Alert::OldBackup(d) => format!("last backup {d} d ago"),
        Alert::DiskFull(p) => format!("disk {p}% full"),
    }
}

fn alert_tag(a: &pxu::share::storage::Alert) -> &'static str {
    use pxu::share::storage::Alert;
    match a {
        Alert::NoBackup => "no-bk",
        Alert::OldBackup(_) => "old-bk",
        Alert::DiskFull(_) => "full",
    }
}

fn storage_info(s: &pxu::share::storage::StorageInfo, wide: bool, partial: bool) -> String {
    let mut v: Vec<String> = vec![];
    if !s.active {
        return "inactive".into();
    }
    if partial {
        v.push("backups…".into());
    }
    if let Some(e) = &s.error {
        v.push(format!("content unreadable ({e})"));
    }
    if let Some(d) = s.dedup {
        v.push(format!("dedup {d:.1}×"));
    }
    if let Some(a) = s.allocated {
        v.push(format!("alloc {} ({})", pxu::share::storage::fmt_gb(a), pct_text(s.alloc_pct())));
    }
    if s.has_backups && s.error.is_none() && !partial {
        v.push(if s.kind == "pbs" && wide { format!("{} bk · {} logical", s.backups, pxu::share::storage::fmt_gb(s.logical)) } else { format!("{} bk · {}", s.backups, pxu::share::storage::fmt_gb(s.logical)) });
    }
    if s.pxu_target {
        v.push(if wide { "pxu pre-update target" } else { "pxu target" }.into());
    }
    if s.templates > 0 {
        v.push(format!("tmpl {}", pxu::share::storage::fmt_gb(s.templates)));
    }
    v.join(" · ")
}

fn draw_storage(f: &mut Frame, app: &App, th: &Theme, area: Rect) {
    use pxu::share::storage::{Alert, GuestUse};
    let w = area.width as usize;
    let st = &app.storage;
    let detail = st.detail.and_then(|id| st.data.as_ref().and_then(|d| d.guests.iter().find(|g| g.vmid == id)));
    let keys = if detail.is_some() {
        if st.backups().is_empty() {
            keybar_wrap(th, &[("↑↓/PgUp/PgDn", "scroll"), ("g/G", "top/end"), ("r", "refresh"), ("Esc/q", "back"), ("?", "help")], 2, w, 2)
        } else {
            keybar_wrap(th, &[("↑↓", "select backup"), ("x", "delete backup"), ("PgUp/PgDn", "scroll"), ("g/G", "top/end"), ("r", "refresh"), ("Esc/q", "back"), ("?", "help")], 2, w, 2)
        }
    } else {
        keybar_wrap(th, &[("↑↓", "move"), ("Enter", "detail"), ("S", "sort"), ("f", "filter (alerts)"), ("r", "refresh"), ("?", "help"), ("q", "back")], 2, w, 2)
    };
    let data = st.data.as_ref();
    let n_st = data.map_or(0, |d| d.storages.len().min(6));
    let n_al = data.map_or(0, |d| d.storage_alerts.len().min(2));
    let head = Constraint::Length(1);
    let parts = if detail.is_some() {
        Layout::vertical([head, head, head, Constraint::Min(1), head, head, head, Constraint::Length(keys.len() as u16)]).split(area)
    } else {
        Layout::vertical([head, head, Constraint::Length(1 + n_st as u16 + n_al as u16), head, head, Constraint::Min(1), head, head, head, head, Constraint::Length(keys.len() as u16)]).split(area)
    };
    let title = match detail {
        Some(g) => format!("pxu · storage · {} {}", g.vmid, g.name),
        None => "pxu · storage".to_string(),
    };
    f.render_widget(Paragraph::new(two_sided(vec![Span::styled(title, th.bold()), Span::styled(badges(app), th.warn())], app.clock.clone(), area.width)), parts[0]);
    let loading = if st.loading { " · reading…" } else { "" };
    let sub = match data {
        Some(d) => format!("node {} · {} storages · {} guests{loading}", d.node, d.storages.len(), d.guests.len()),
        None => format!("node {}{}", app.inv.node, if st.loading { " · reading storages…" } else { "" }),
    };
    let rule_line = || Paragraph::new("─".repeat(w)).style(th.muted());
    let status_text = |fallback: String| if app.status.is_empty() { fallback } else { app.status.clone() };

    if let Some(g) = detail {
        let d = data.unwrap();
        let alerts = if g.alerts.is_empty() { "no alerts".to_string() } else { g.alerts.iter().map(alert_text).collect::<Vec<_>>().join(" · ") };
        let kind = match g.kind {
            GuestKind::Lxc => "LXC",
            GuestKind::Vm => "VM",
            GuestKind::Host => "host",
        };
        let span = Line::from(vec![Span::styled(trunc(&format!("{kind} · {}{loading} · ", state_label_of(g.state)), w), th.muted()), Span::styled(trunc(&alerts, w.saturating_sub(14)), if g.alerts.is_empty() { th.ok() } else { th.warn() })]);
        f.render_widget(Paragraph::new(span), parts[1]);
        f.render_widget(rule_line(), parts[2]);
        let (lines, cur) = storage_detail_lines(g, d, th, w, st.bsel);
        let h = parts[3].height as usize;
        st.body_h.set(h);
        st.detail_len.set(lines.len());
        let mut top = st.top.get().min(lines.len().saturating_sub(h));
        if st.follow.take() && !st.backups().is_empty() {
            // el cursor de Backups a la vista
            if cur < top {
                top = cur;
            } else if cur >= top + h {
                top = cur + 1 - h;
            }
        }
        st.top.set(top);
        f.render_widget(Paragraph::new(lines.iter().skip(top).take(h).cloned().collect::<Vec<_>>()), parts[3]);
        f.render_widget(rule_line(), parts[4]);
        f.render_widget(Paragraph::new(trunc(&status_text(format!("lines {}-{} of {}", top + 1, (top + h).min(lines.len()), lines.len())), w)).style(th.accent()), parts[5]);
        f.render_widget(rule_line(), parts[6]);
        f.render_widget(Paragraph::new(keys), parts[7]);
        return;
    }

    f.render_widget(Paragraph::new(trunc(&sub, w)).style(th.muted()), parts[1]);
    let Some(d) = data else {
        f.render_widget(Paragraph::new(if st.loading { " reading storages, backups and thin pools…" } else { " no data yet (r reads again)" }).style(th.muted()), parts[5]);
        f.render_widget(rule_line(), parts[8]);
        f.render_widget(Paragraph::new(keys), parts[10]);
        return;
    };

    // --- Storages
    let mut lines: Vec<Line> = vec![rule_spans(&[("Storages", th.accent())], w, th)];
    let barw = if w >= 100 { 12 } else { 8 };
    let nw = d.storages.iter().take(6).map(|s| s.name.chars().count()).max().unwrap_or(0).clamp(10, 18);
    for s in d.storages.iter().take(6) {
        let p = s.pct();
        let used = if s.active { format!("{}/{}", sz(s.used), sz(s.total)) } else { "—".into() };
        let mut spans = vec![Span::raw(format!(" {} {} ", left(&s.name, nw), left(&s.kind, 7)))];
        if s.active {
            spans.push(Span::styled(bar(p, barw), use_style(th, p)));
        } else {
            spans.push(Span::styled(format!("[{}]", "·".repeat(barw)), th.muted()));
        }
        spans.push(Span::raw(format!(" {} {}  ", right(&used, 11), right(&if s.active { pct_text(p) } else { "—".into() }, 4))));
        let used_w: usize = 1 + nw + 1 + 7 + 1 + barw + 2 + 1 + 11 + 1 + 4 + 2;
        spans.push(Span::styled(trunc(&storage_info(s, w >= 100, d.partial), w.saturating_sub(used_w)), if s.active { Style::default() } else { th.muted() }));
        lines.push(Line::from(spans));
    }
    for a in d.storage_alerts.iter().take(2) {
        lines.push(Line::styled(trunc(&format!(" ! {a}"), w), th.warn()));
    }
    f.render_widget(Paragraph::new(lines), parts[2]);

    // --- Guests
    let wide = w >= 100;
    let alerts_only = if st.only_alerts { " · alerts only" } else { "" };
    f.render_widget(
        Paragraph::new(rule_spans(&[("Guests", th.accent()), (" · sort: ", th.muted()), (st.sort.label(), th.fg(Color::White)), (alerts_only, th.muted())], w, th)),
        parts[3],
    );
    // ancho fijo de la fila: 65 con STATE, 57 sin él (+ nombre y alerta); la alerta se queda con lo que sobra
    let longest = d.guests.iter().map(|g| g.name.chars().count()).max().unwrap_or(0);
    // WHERE (storage del último backup): solo en pantallas anchas, como STATE; el ancho sigue al nombre más largo
    let where_w = if wide { d.guests.iter().filter_map(|g| g.last_backup()).map(|b| b.storage.chars().count()).max().unwrap_or(0).clamp(5, 15) } else { 0 };
    let name_w = if wide { longest.clamp(8, 20).min(w.saturating_sub(65 + where_w + 1 + 14).max(8)) } else { longest.clamp(8, 16).min(w.saturating_sub(64).max(8)) };
    let row_text = |marker: &str, id: &str, name: &str, state: &str, disk: &str, pct: &str, snaps: &str, last: &str, wher: &str, bsize: &str, bk: &str, alert: &str| {
        let alert_w = if wide { w.saturating_sub(65 + where_w + 1 + name_w) } else { 6 };
        let mut s = format!("{marker}{} {}", right(id, 4), left(name, name_w));
        if wide {
            s += &format!(" {}", left(state, 7));
        }
        s += &format!(" {} {} {} {}", right(disk, 11), right(pct, 4), right(snaps, 5), left(last, 11));
        if wide {
            s += &format!(" {}", left(&trunc(wher, where_w), where_w));
        }
        s += &format!(" {} {} {}", right(bsize, 7), left(bk, 6), left(alert, alert_w));
        trunc(&s, w)
    };
    f.render_widget(Paragraph::new(row_text(" ", "ID", "NAME", "STATE", "USED/ALLOC", "%", "SNAPS", "LAST BACKUP", "WHERE", "BK SIZE", "BACKUP", "ALERT")).style(th.bold()), parts[4]);
    let list = d.view(st.sort, st.only_alerts);
    let h = parts[5].height as usize;
    st.rows_h.set(h);
    if list.is_empty() {
        st.off.set(0);
        f.render_widget(Paragraph::new(if st.only_alerts { " no guest has alerts" } else { " no guests on this node" }).style(th.muted()), parts[5]);
    } else {
        let sel = st.sel.min(list.len() - 1);
        let mut off = st.off.get().min(list.len() - 1);
        if sel < off {
            off = sel;
        } else if sel >= off + h {
            off = sel + 1 - h;
        }
        st.off.set(off);
        for (n, (i, g)) in list.iter().enumerate().skip(off).take(h).enumerate() {
            let g: &GuestUse = g;
            let row = Rect { x: parts[5].x, y: parts[5].y + n as u16, width: parts[5].width, height: 1 };
            let last = g.last_backup();
            let alert = if wide { g.alerts.iter().map(alert_text).collect::<Vec<_>>().join(", ") } else { g.alerts.first().map_or(String::new(), |a| alert_tag(a).into()) };
            let disk = format!("{}/{}", sz(g.used), sz(g.alloc));
            let snaps = if g.snaps.is_empty() { "0".to_string() } else { format!("{}{}", g.snaps.len(), if g.pxu_snaps > 0 { "*" } else { "" }) };
            let line = row_text(
                if g.has_alert() { "!" } else { " " },
                &g.vmid.to_string(),
                &g.name,
                state_label_of(g.state),
                &disk,
                &pct_text(g.pct()),
                &snaps,
                &if d.partial { "…".to_string() } else { last.and_then(|b| b.ctime).map_or("—".to_string(), |t| pxu::share::storage::when(Some(t), d.now)[..10].to_string()) },
                &if d.partial { "…".to_string() } else { last.map_or("—".to_string(), |b| b.storage.clone()) },
                &if d.partial { "…".to_string() } else { last.map_or("—".to_string(), |b| pxu::share::storage::fmt_gb(b.size)) },
                backup_cell(g.kind, &g.pre_update),
                &alert,
            );
            let style = if i == sel {
                th.sel()
            } else if g.has_alert() {
                th.warn()
            } else if g.state != RunState::Running {
                th.muted()
            } else {
                Style::default()
            };
            f.render_widget(Paragraph::new(line).style(style), row);
        }
    }

    // --- resumen de alertas
    let count = |f: fn(&Alert) -> bool| d.count(f);
    let sum = if d.any_alert() {
        let mut v: Vec<String> = vec![];
        for (n, label, ids) in [
            (count(|a| matches!(a, Alert::NoBackup)), "no backup".to_string(), d.ids(|a| matches!(a, Alert::NoBackup))),
            (count(|a| matches!(a, Alert::OldBackup(_))), format!("backup > {} d", pxu::share::storage::OLD_BACKUP_DAYS), d.ids(|a| matches!(a, Alert::OldBackup(_)))),
            (count(|a| matches!(a, Alert::DiskFull(_))), format!("disk >= {}%", pxu::share::storage::FULL_DISK_PCT), d.ids(|a| matches!(a, Alert::DiskFull(_)))),
        ] {
            if n > 0 {
                let ids = ids.iter().take(3).map(|i| i.to_string()).collect::<Vec<_>>().join(",");
                v.push(format!("{label}: {n} ({ids}{})", if n > 3 { ",…" } else { "" }));
            }
        }
        if !d.storage_alerts.is_empty() {
            v.push(format!("storage: {}", d.storage_alerts.len()));
        }
        format!("alerts · {}", v.join(" · "))
    } else if d.guests.is_empty() && d.storages.is_empty() {
        format!("nothing readable{}", d.errors.first().map_or(String::new(), |e| format!(" · {e}")))
    } else {
        "no alerts".to_string()
    };
    let sum_style = if d.any_alert() { th.warn() } else if d.storages.is_empty() { th.muted() } else { th.ok() };
    f.render_widget(Paragraph::new(trunc(&format!(" {sum}"), w)).style(sum_style), parts[6]);
    f.render_widget(rule_line(), parts[7]);
    let ind = if list.is_empty() { String::new() } else { format!("{} / {}", st.sel.min(list.len() - 1) + 1, list.len()) };
    let ind = match d.errors.first() {
        Some(e) if app.status.is_empty() => format!("{ind}   unreadable: {e}"),
        _ => ind,
    };
    f.render_widget(Paragraph::new(trunc(&status_text(ind), w)).style(th.accent()), parts[8]);
    f.render_widget(rule_line(), parts[9]);
    f.render_widget(Paragraph::new(keys), parts[10]);
}

/// Celda BACKUP (¿pxu hace backup antes de actualizar?): la usan la lista principal y Storage.
pub(crate) fn backup_cell(kind: GuestKind, mode: &str) -> &'static str {
    if kind == GuestKind::Lxc && mode != "none" {
        "yes"
    } else {
        "-"
    }
}

fn state_label_of(s: RunState) -> &'static str {
    match s {
        RunState::Running => "running",
        RunState::Stopped => "stopped",
        _ => "unknown",
    }
}

/// Detalle de espacio de un guest: discos, snapshots (`pxu-*` marcados), backups de todos los storages y el modo previo al update.
fn storage_detail_lines(g: &pxu::share::storage::GuestUse, d: &pxu::share::storage::StorageData, th: &Theme, w: usize, bsel: usize) -> (Vec<Line<'static>>, usize) {
    use pxu::share::storage::{fmt_gb, when};
    const SNAP_PREFIX_NOTE: &str = "(pxu pre-update snapshot)";
    let mut v: Vec<Line<'static>> = vec![];
    let l = |s: String| Line::raw(trunc(&s, w));
    v.push(Line::styled(rule(&format!("Disks · rootfs {} used of {} · {}", sz(g.used), sz(g.alloc), pct_text(g.pct())), w), th.muted()));
    if g.disks.is_empty() {
        v.push(l("  no disk on a storage (bind mounts and CD-ROMs do not use one)".into()));
    }
    for k in &g.disks {
        let p = k.pct();
        let mount = k.mount.as_deref().map_or(String::new(), |m| format!("  mp={m}"));
        v.push(Line::styled(trunc(&format!("  {:<7} {}  {} / {}  {}{mount}", k.slot, k.volid, sz(k.used), sz(k.alloc), pct_text(p)), w), use_style(th, p)));
        // bloques del thin pool: dato secundario (incluye archivos borrados sin trim), solo aquí
        if let Some((t, tu)) = k.thin_pct.zip(k.thin_used).filter(|_| k.untrimmed() || k.used.is_none()) {
            let hint = if k.untrimmed() && g.kind == GuestKind::Lxc && k.slot == "rootfs" { format!(" — reclaim with `pct fstrim {}`", g.vmid) } else { String::new() };
            v.push(Line::styled(trunc(&format!("          thin blocks {t:.1}% ({}){hint}", fmt_gb(tu)), w), if k.untrimmed() { th.warn() } else { th.muted() }));
        }
    }
    v.push(Line::default());
    v.push(Line::styled(rule(&format!("Snapshots ({})", g.snaps.len()), w), th.muted()));
    if g.snaps.is_empty() {
        v.push(l("  none".into()));
    }
    for s in &g.snaps {
        v.push(if s.starts_with(pxu::share::update::backup::SNAP_PREFIX) { Line::styled(trunc(&format!("  {s}  {SNAP_PREFIX_NOTE}"), w), th.accent()) } else { l(format!("  {s}")) });
    }
    v.push(Line::default());
    v.push(Line::styled(rule(&format!("Backups ({}, all backup storages)", g.backups.len()), w), th.muted()));
    if g.backups.is_empty() {
        v.push(l("  none found".into()));
    }
    let mut cur = 0;
    for (i, b) in g.backups.iter().enumerate() {
        let tag = format!("{}{}", if b.protected { "(protected) " } else { "" }, if b.pxu { "(pxu pre-update) " } else { "" });
        let on = i == bsel.min(g.backups.len() - 1);
        let text = format!("{} {:<18} {:>7}  {:<10} {tag}{}", if on { " >" } else { "  " }, when(b.ctime, d.now), fmt_gb(b.size), b.storage, b.volid.split_once(':').map_or(b.volid.as_str(), |x| x.1));
        if on {
            cur = v.len();
            v.push(Line::styled(trunc(&text, w), th.sel()));
        } else {
            v.push(l(text));
        }
    }
    v.push(Line::default());
    v.push(Line::styled(rule("Pre-update backup (pxu)", w), th.muted()));
    if g.kind == GuestKind::Lxc {
        v.push(l(format!("  mode: {}", g.pre_update)));
        v.push(l(format!("  origin: {}", g.pre_update_why)));
    } else {
        v.push(l("  n/a: pxu updates LXC containers only".into()));
    }
    (v, cur)
}

/// `d` en Config: días de retención (0 = no borrar nunca).
fn draw_log_modal(f: &mut Frame, app: &App, th: &Theme, area: Rect) {
    let Some(t) = &app.log_modal else { return };
    let w = 64.min(area.width.saturating_sub(4));
    let r = Rect { x: (area.width - w) / 2, y: area.height.saturating_sub(7) / 2, width: w, height: 7.min(area.height) };
    f.render_widget(Clear, r);
    let blk = Block::default().borders(Borders::ALL).title(Span::styled(" log retention ", th.bold())).border_style(th.accent());
    let inner = blk.inner(r);
    f.render_widget(blk, r);
    let preview = match t.parse::<u32>() {
        Ok(d) => format!("will write [general] log_retention_days = {d}"),
        Err(_) => "type a number of days".to_string(),
    };
    let lines = vec![
        Line::raw(" days to keep run logs (0 = never delete):"),
        Line::from(vec![Span::raw(" > "), Span::styled(format!("{t}_"), th.accent())]),
        Line::styled(format!(" {preview}"), th.muted()),
        Line::default(),
        mbar(th, &[("Enter", "save"), ("Esc", "cancel")], 0, inner.width as usize),
    ];
    f.render_widget(Paragraph::new(lines), inner);
}

/// Área de contenido de Config (marco entre la cabecera y el pie): las confirmaciones se dibujan dentro y no tapan el pie.
fn config_body(th: &Theme, area: Rect) -> Rect {
    let keys = config_keys(th, area.width);
    Layout::vertical([Constraint::Length(1), Constraint::Length(1), Constraint::Length(1), Constraint::Min(3), Constraint::Length(1), Constraint::Length(1), Constraint::Length(1), Constraint::Length(keys.len() as u16 + 1)]).split(area)[3]
}

fn config_keys(th: &Theme, w: u16) -> Vec<Line<'static>> {
    keybar_wrap(th, &[("s", "schedule"), ("n", "notifications"), ("d", "log days"), ("l", "logs"), ("t", "test"), ("r", "re-read"), ("Esc", "back"), ("?", "help"), ("q", "quit")], 2, w as usize, 2)
}

/// Parte `text` en filas de `width` columnas; las continuaciones llevan `indent` espacios (sangría colgante).
fn wrap_hanging(text: &str, width: usize, indent: usize) -> Vec<String> {
    if text.chars().count() <= width {
        return vec![text.to_string()];
    }
    let mut rows: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in text.split(' ').filter(|w| !w.is_empty()) {
        if !cur.is_empty() && cur.chars().count() + 1 + word.chars().count() > width {
            rows.push(std::mem::replace(&mut cur, " ".repeat(indent)));
        }
        if cur.is_empty() {
            cur = format!("{}{word}", " ".repeat(text.chars().take_while(|c| *c == ' ').count()));
        } else {
            if !cur.trim().is_empty() {
                cur.push(' ');
            }
            cur.push_str(word);
        }
    }
    rows.push(cur);
    rows
}

/// Líneas de una confirmación (`today`/`new`, campos, notas): margen de 1 columna, la marca de cambio se cambia por un
/// espacio (las etiquetas siguen en la misma columna) y se ven en negrita; las notas largas se parten con sangría colgante.
fn confirm_lines(th: &Theme, raw: &[String], width: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for l in raw {
        let changed = l.starts_with(pxu::share::config::CHANGED_MARK);
        let l = l.replacen(pxu::share::config::CHANGED_MARK, " ", 1);
        if l.is_empty() {
            out.push(Line::default());
            continue;
        }
        let lead = l.chars().take_while(|c| *c == ' ').count();
        let indent = 1 + lead + if l.trim_start().starts_with('·') { 2 } else { 0 };
        for row in wrap_hanging(&format!(" {l}"), width, indent) {
            out.push(if changed {
                Line::styled(row, th.bold())
            } else if l == "today" || l == "new" {
                Line::styled(row, th.muted())
            } else {
                Line::raw(row)
            });
        }
    }
    out
}

/// Ajusta `lines` (la última es la barra de teclas) a `budget` filas: quita primero las líneas en blanco (la de arriba,
/// luego las que separan bloques), después la nota «the rest of…» y, si aún no cabe, recorta el final con «…».
fn fit_rows(mut lines: Vec<Line<'static>>, budget: usize) -> Vec<Line<'static>> {
    let blank = |l: &Line| l.to_string().trim().is_empty();
    while lines.len() > budget {
        let Some(i) = lines[..lines.len() - 1].iter().position(blank) else { break };
        lines.remove(i);
    }
    if lines.len() > budget {
        if let Some(i) = lines.iter().position(|l| l.to_string().trim_start().starts_with("· the rest of")) {
            lines.remove(i);
        }
    }
    if lines.len() > budget && budget >= 2 {
        let bar = lines.pop().unwrap();
        lines.truncate(budget - 1);
        if let Some(last) = lines.last_mut() {
            *last = Line::raw(" …");
        }
        lines.push(bar);
    }
    lines
}

/// Área de una confirmación: dentro del contenido de Config (no tapa cabecera ni pie); en otras vistas, toda la pantalla.
fn confirm_area(app: &App, th: &Theme, area: Rect) -> Rect {
    if app.view == View::Config { config_body(th, area) } else { area }
}

fn draw_sched_modal(f: &mut Frame, app: &App, th: &Theme, full: Rect) {
    let area = if app.sched.as_ref().is_some_and(|m| m.stage == super::sched::Stage::Confirm) { confirm_area(app, th, full) } else { full };
    let iw = centered(area, 100, 3).width.saturating_sub(2).max(1) as usize;
    use super::sched::{Field, Stage};
    let Some(m) = &app.sched else { return };
    let mut lines: Vec<Line> = Vec::new();
    let confirm = m.stage == Stage::Confirm;
    let (title, bottom): (&str, &[(&str, &str)]) = if confirm {
        (" Save the schedule? ", &[("y/Enter", "save"), ("b", "back"), ("Esc", "cancel")])
    } else {
        (" Schedule the automatic update ", &[("Tab", "field"), ("←/→/Space", "change"), ("Enter", "validate"), ("Esc", "cancel")])
    };
    if confirm {
        lines.push(Line::default());
        lines.extend(confirm_lines(th, &m.lines, iw));
    } else {
        lines.push(Line::raw(format!(" today: {}   (all times are the server's)", m.today())));
        lines.push(Line::default());
        let cur = m.fields().get(m.field).copied();
        for fl in m.fields() {
            let mark = if Some(fl) == cur { ">" } else { " " };
            let (name, val) = match fl {
                Field::Enable => ("Automatic update", if m.enable { "[x] enabled".to_string() } else { "[ ] disabled".to_string() }),
                Field::Preset => ("Frequency", m.preset.label().to_string()),
                Field::Day => ("Day", pxu::share::schedule::DAYS[m.day % 7].to_string()),
                Field::Time => ("Time (HH:MM)", format!("{}{}", m.time, if Some(fl) == cur { "_" } else { "" })),
                Field::Custom => ("OnCalendar", format!("{}{}", m.custom, if Some(fl) == cur { "_" } else { "" })),
            };
            let t = format!(" {mark} {name:<18} {val}");
            lines.push(if Some(fl) == cur { Line::styled(t, th.sel()) } else { Line::raw(t) });
        }
        if let Some(e) = &m.error {
            lines.push(Line::default());
            lines.push(Line::styled(format!(" ! {e}"), th.warn()));
        }
    }
    if !m.warnings.is_empty() {
        lines.push(Line::default());
    }
    lines.extend(m.warnings.iter().map(|wn| Line::styled(format!(" ! {wn}"), th.warn())));
    lines.push(Line::default());
    lines.push(mbar(th, bottom, 4, 96));
    if confirm {
        lines = fit_rows(lines, area.height.saturating_sub(2) as usize);
    }
    let rows: usize = lines.iter().map(|l| l.to_string().chars().count().div_ceil(iw).max(1)).sum();
    let r = centered(area, 100, (rows as u16 + 2).min(if confirm { area.height } else { area.height.saturating_sub(2) }));
    f.render_widget(Clear, r);
    let blk = Block::default().borders(Borders::ALL).title(Span::styled(title, th.bold())).border_style(th.accent());
    let inner = blk.inner(r);
    f.render_widget(blk, r);
    f.render_widget(Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: false }), inner);
}

fn draw_apprise_modal(f: &mut Frame, app: &App, th: &Theme, full: Rect) {
    let area = if app.apprise_modal.as_ref().is_some_and(|m| m.stage == super::apprise::Stage::Confirm) { confirm_area(app, th, full) } else { full };
    let iw = centered(area, 100, 3).width.saturating_sub(2).max(1) as usize;
    use super::apprise::{Field, Stage};
    use pxu::share::config::{AppriseMode, APPRISE_EVENTS};
    let Some(m) = &app.apprise_modal else { return };
    let mut lines: Vec<Line> = Vec::new();
    let confirm = m.stage == Stage::Confirm;
    let (title, bottom): (&str, &[(&str, &str)]) = if confirm {
        (" Save the notifications? ", &[("y/Enter", "save"), ("b", "back"), ("Esc", "cancel")])
    } else {
        (" Notifications (Apprise) ", &[("Tab", "field"), ("←/→/Space", "change"), ("Enter", "validate"), ("Esc", "cancel")])
    };
    if confirm {
        lines.push(Line::default());
        lines.extend(confirm_lines(th, &m.lines, iw));
    } else {
        let cur = m.fields().get(m.field).copied();
        let dots = |n: usize| "•".repeat(n.min(40));
        for fl in m.fields() {
            let mark = if Some(fl) == cur { ">" } else { " " };
            let c = if Some(fl) == cur { "_" } else { "" };
            let (name, val) = match fl {
                Field::Url => ("API address".to_string(), format!("{}{c}", m.api_url)),
                Field::Mode => ("Sends with".to_string(), if m.mode == AppriseMode::Key { "(•) saved key in Apprise API  ( ) urls".to_string() } else { "( ) saved key in Apprise API  (•) urls".to_string() }),
                Field::Secret => {
                    let n = if m.mode == AppriseMode::Key { "API key" } else { "URLs (comma-separated)" };
                    let v = if m.secret.is_empty() && m.stored() { "(stored — leave empty to keep)".to_string() } else { format!("{}{c}", dots(m.secret.chars().count())) };
                    (n.to_string(), v)
                }
                Field::Tag => ("Tag".to_string(), if m.tag.is_empty() && Some(fl) != cur { "(none)".to_string() } else { format!("{}{c}", m.tag) }),
                Field::Event(i) => (if i == 0 { "Notify on".to_string() } else { String::new() }, format!("[{}] {}", if m.on[i] { "x" } else { " " }, APPRISE_EVENTS[i])),
            };
            let t = format!(" {mark} {name:<24} {val}");
            lines.push(if Some(fl) == cur { Line::styled(t, th.sel()) } else { Line::raw(t) });
        }
        lines.push(Line::default());
        lines.push(Line::styled(" secrets are never shown (dots) and are saved encrypted (systemd-creds)", th.muted()));
        lines.push(Line::styled(" tag empty: only untagged targets of the key are notified", th.muted()));
        if let Some(e) = &m.error {
            lines.push(Line::default());
            lines.push(Line::styled(format!(" ! {e}"), th.warn()));
        }
    }
    lines.push(Line::default());
    lines.push(mbar(th, bottom, 4, 96));
    if confirm {
        lines = fit_rows(lines, area.height.saturating_sub(2) as usize);
    }
    let rows: usize = lines.iter().map(|l| l.to_string().chars().count().div_ceil(iw).max(1)).sum();
    let r = centered(area, 100, (rows as u16 + 2).min(if confirm { area.height } else { area.height.saturating_sub(2) }));
    f.render_widget(Clear, r);
    let blk = Block::default().borders(Borders::ALL).title(Span::styled(title, th.bold())).border_style(th.accent());
    let inner = blk.inner(r);
    f.render_widget(blk, r);
    f.render_widget(Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: false }), inner);
}

/// ¿Está el CT `v` antes de instalar (pre-checks, backup, refresco)? Entonces cancelar lo corta en el acto y lo deja intacto.
fn cancel_stops_now(u: &super::app::UpdateState, v: u32) -> bool {
    u.queue.iter().find(|q| q.vmid == v).is_some_and(|q| matches!(q.state, QState::Active(s) if s.before_install()))
}

fn draw_cancel(f: &mut Frame, app: &App, th: &Theme, area: Rect) {
    let r = centered(area, 66, 7);
    f.render_widget(Clear, r);
    let blk = Block::default().borders(Borders::ALL).title(Span::styled(" Cancel run? ", th.bold())).border_style(th.warn());
    let inner = blk.inner(r);
    f.render_widget(blk, r);
    let cur = app.update.current.map_or("the CT in progress".to_string(), |v| format!("CT {v}"));
    let now = app.update.current.is_some_and(|v| cancel_stops_now(&app.update, v));
    let fin = if now { format!("stop now: abort {cur}'s backup, clean up") } else { format!("finish {cur} (upgrade → verify → restore), then stop") };
    let note = if now { " nothing installed yet, CT untouched · pending CTs become canceled" } else { " dpkg/apt is never killed mid-install · pending CTs become canceled" };
    f.render_widget(
        Paragraph::new(vec![
            mbar(th, &[("Enter", &fin)], 1, 200),
            mbar(th, &[("Esc", "keep running")], 1, 200),
            Line::default(),
            Line::styled(note, th.muted()),
        ]),
        inner,
    );
}

fn rule(title: &str, w: usize) -> String {
    let t = format!("── {title} ");
    let n = t.chars().count();
    format!("{t}{}", "─".repeat(w.saturating_sub(n)))
}

/// Como `rule`, pero con estilo por tramo del título; `──` y el relleno van en `muted`. Mismo ancho que `rule`.
fn rule_spans(parts: &[(&str, Style)], w: usize, th: &Theme) -> Line<'static> {
    let mut spans = vec![Span::styled("── ", th.muted())];
    let mut n = 3;
    for (t, st) in parts {
        n += t.chars().count();
        spans.push(Span::styled(t.to_string(), *st));
    }
    n += 1;
    spans.push(Span::styled(format!(" {}", "─".repeat(w.saturating_sub(n))), th.muted()));
    Line::from(spans)
}

fn mmss(s: u64) -> String {
    if s >= 3600 { format!("{}h{:02}m", s / 3600, s % 3600 / 60) } else { format!("{}m{:02}s", s / 60, s % 60) }
}

fn queue_line(q: &QItem, now: u64, th: &Theme, w: usize) -> Line<'static> {
    let (glyph, label, time, style) = match q.state {
        QState::Pending => ("·", "pending".to_string(), String::new(), th.muted()),
        QState::Active(s) => ("●", s.label().to_string(), { let e = now.saturating_sub(q.started); format!("{:02}:{:02}", e / 60, e % 60) }, th.accent()),
        QState::Done(o) => match o {
            Outcome::Ok => ("✓", "ok".into(), mmss(q.took_s), th.ok()),
            Outcome::Warn => ("✓", "warn".into(), mmss(q.took_s), th.warn()),
            Outcome::Failed => ("✗", "FAIL".into(), mmss(q.took_s), th.warn().add_modifier(Modifier::BOLD)),
            Outcome::Skipped => ("»", "skip".into(), "—".into(), th.muted()),
            Outcome::Cancelled => ("»", "canceled".into(), "—".into(), th.muted()),
        },
    };
    let t = format!("{glyph} {:<5} {:<16} {:<16} {:>6}  {}", q.vmid, trunc(&q.name, 16), label, time, q.detail);
    Line::styled(trunc(&t, w), style)
}

fn draw_update(f: &mut Frame, app: &App, th: &Theme, area: Rect) {
    let (w, u) = (area.width, &app.update);
    let q_h = (u.queue.len() as u16).clamp(1, (area.height / 3).max(3));
    let keys = if u.running {
        keybar_wrap(th, &[("x", "cancel"), ("↑↓", "scroll log"), ("f", "follow"), ("?", "help")], 1, w as usize, 2)
    } else {
        keybar_wrap(th, &[("Esc", "back"), ("↑↓", "scroll log"), ("f", "follow"), ("?", "help"), ("q", "quit")], 2, w as usize, 2)
    };
    let parts = Layout::vertical([
        Constraint::Length(1), // header
        Constraint::Length(1), // resumen
        Constraint::Length(1), // ── queue
        Constraint::Length(q_h),
        Constraint::Length(1), // ── log
        Constraint::Min(1),
        Constraint::Length(1), // regla
        Constraint::Length(1), // cancelación
        Constraint::Length(1), // estado
        Constraint::Length(1), // regla
        Constraint::Length(keys.len() as u16), // teclas
    ])
    .split(area);
    let count = |o: Outcome| u.queue.iter().filter(|q| q.state == QState::Done(o)).count();
    let done = u.queue.iter().filter(|q| matches!(q.state, QState::Done(_))).count();
    let right = format!("started {} · sequential   {}", u.started, app.clock);
    f.render_widget(Paragraph::new(two_sided(vec![Span::styled("pxu · update run", th.bold()), Span::styled(badges(app), th.muted())], right, w)), parts[0]);
    let state = if u.stop_requested { " · stopping after the CT in progress" } else if u.running { "" } else if u.failed.is_some() { " · failed to start" } else { " · finished" };
    let sum = format!("ok {} · warn {} · fail {} · skip {} · canceled {} · CT {}/{}{}", count(Outcome::Ok), count(Outcome::Warn), count(Outcome::Failed), count(Outcome::Skipped), count(Outcome::Cancelled), done, u.queue.len(), state);
    f.render_widget(Paragraph::new(trunc(&sum, w as usize)).style(th.warn()), parts[1]);
    f.render_widget(Paragraph::new(rule("queue", w as usize)).style(th.accent()), parts[2]);
    // La cola sigue al CT activo (o al último terminado).
    let focus = u.queue.iter().position(|q| matches!(q.state, QState::Active(_))).or_else(|| u.queue.iter().rposition(|q| matches!(q.state, QState::Done(_)))).unwrap_or(0);
    let top = (focus + 1).saturating_sub(q_h as usize);
    let mut ql: Vec<Line> = u.queue.iter().skip(top).take(q_h as usize).map(|q| queue_line(q, app.now_epoch, th, w as usize)).collect();
    if ql.is_empty() {
        ql.push(Line::styled(if u.running { "  selecting…" } else { "  (nothing ran)" }, th.muted()));
    }
    f.render_widget(Paragraph::new(ql), parts[3]);
    let who = u.current.and_then(|v| u.queue.iter().find(|q| q.vmid == v)).map_or(String::new(), |q| format!(" · {} {}", q.vmid, q.name));
    f.render_widget(Paragraph::new(rule(&format!("live log{who}"), w as usize)).style(th.accent()), parts[4]);
    let h = parts[5].height as usize;
    let total = u.log.len();
    let reserve = if u.scroll > 0 || u.dropped > 0 { 1 } else { 0 };
    let rows = h.saturating_sub(reserve);
    let end = total.saturating_sub(u.scroll);
    let start = end.saturating_sub(rows);
    let mut ll: Vec<Line> = u.log.iter().skip(start).take(end - start).map(|(t, v, l)| Line::raw(trunc(&format!(" {t}  {v:>3} {l}"), w as usize))).collect();
    if reserve == 1 {
        let older = start + u.dropped;
        ll.push(Line::styled(trunc(&format!(" … {older} older lines · {} newer · [↑↓] scroll · [f] follow · ring keeps {} lines", total - end, super::app::LOG_RING), w as usize), th.muted()));
    }
    f.render_widget(Paragraph::new(ll), parts[5]);
    let cancel = if u.running {
        match u.current {
            Some(v) if cancel_stops_now(u, v) => format!("cancel: [x] → stop now: CT {v} is before the install, its backup is aborted and cleaned up, CT untouched"),
            Some(v) => format!("cancel: [x] → finish current guest ({v}), then stop · killing dpkg mid-install is never done"),
            None => "cancel: [x] → stop after the current step".into(),
        }
    } else if let Some(r) = &u.report {
        format!("{}{}", r.aborted.as_deref().map(|a| format!("run stopped: {a} · ")).unwrap_or_default(), u.notes.first().map_or("no notification sent".to_string(), |n| format!("notify: {n}")))
    } else {
        u.failed.clone().unwrap_or_default()
    };
    f.render_widget(Paragraph::new(trunc(&cancel, w as usize)).style(th.warn()), parts[7]);
    f.render_widget(Paragraph::new("─".repeat(w as usize)).style(th.muted()), parts[6]);
    let st = if !app.status.is_empty() { app.status.clone() } else { "step times update on log lines or state changes · log: /var/log/pxu/run-*.log".to_string() };
    f.render_widget(Paragraph::new(trunc(&st, w as usize)).style(th.accent()), parts[8]);
    f.render_widget(Paragraph::new("─".repeat(w as usize)).style(th.muted()), parts[9]);
    f.render_widget(Paragraph::new(keys), parts[10]);
}

#[cfg(test)]
pub fn wrap_for_test(s: &str, w: usize) -> Vec<String> {
    wrap_line(s, w)
}

#[cfg(test)]
mod wrap_tests {
    use super::wrap_hanging;

    #[test]
    fn long_notes_continue_under_the_bullet_text_and_short_lines_keep_their_spacing() {
        let rows = wrap_hanging(" · aaaa bbbb cccc dddd", 14, 4);
        assert_eq!(rows, vec![" · aaaa bbbb", "    cccc dddd"]);
        assert_eq!(wrap_hanging("   api_url   x", 40, 3), vec!["   api_url   x"]);
    }
}
