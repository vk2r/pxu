//! Formato compartido por `pxu list` y la TUI.
pub fn bytes_short(b: u64) -> String {
    let g = b as f64 / (1u64 << 30) as f64;
    if g >= 1024.0 {
        format!("{:.1} TB", g / 1024.0)
    } else if g >= 100.0 {
        format!("{:.0} GB", g)
    } else if g >= 1.0 {
        format!("{:.1} GB", g)
    } else {
        format!("{} MB", b >> 20)
    }
}

/// Todo texto que venga de un CT pasa por aquí antes de mostrarse o registrarse: los caracteres de control
/// (ESC, CSI/OSC, C1, DEL, BEL…) pasan a `·` y el tab a un espacio. El UTF-8 normal no se toca.
pub fn clean(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\t' => ' ',
            c if c.is_control() => '·',
            c => c,
        })
        .collect()
}

/// Una línea con `\r` (barra de progreso que se redibuja en el mismo sitio) se queda con su último estado: cada `\r` descarta lo anterior.
pub fn last_redraw(s: &str) -> &str {
    let s = s.trim_end_matches(['\r', '\n']);
    s.rsplit('\r').find(|p| !p.trim().is_empty()).unwrap_or(s)
}

/// Oculta `usuario:clave@` en URLs (`https://user:pass@host` → `https://***@host`) antes de guardar un log.
pub fn mask_url_creds(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("://") {
        let (head, tail) = rest.split_at(i + 3);
        out.push_str(head);
        let end = tail.find(|c: char| c.is_whitespace() || c == '/').unwrap_or(tail.len());
        match tail[..end].rfind('@') {
            Some(at) if tail[..at].contains(':') => {
                out.push_str("***");
                rest = &tail[at..];
            }
            _ => rest = tail,
        }
    }
    out.push_str(rest);
    out
}

/// Trunca a `w` columnas añadiendo `…` (cuenta chars; el contenido es ASCII/latino). Sanea al entrar.
pub fn trunc(s: &str, w: usize) -> String {
    let s = &clean(s);
    if s.chars().count() <= w {
        return s.to_string();
    }
    if w == 0 {
        return String::new();
    }
    let mut o: String = s.chars().take(w - 1).collect();
    o.push('…');
    o
}

fn gb(b: u64) -> String {
    let g = b as f64 / (1u64 << 30) as f64;
    if g >= 100.0 {
        format!("{g:.0}")
    } else {
        format!("{g:.1}")
    }
}

/// "0.2/0.5 GB" (usado/límite); sin lectura de cgroup, "—/0.5 GB".
pub fn ram(used: Option<u64>, limit_mb: Option<u64>) -> String {
    let lim = limit_mb.map(|m| gb(m << 20)).unwrap_or_else(|| "—".into());
    format!("{}/{lim} GB", used.map(gb).unwrap_or_else(|| "—".into()))
}

pub fn upd(n: usize, sec: usize, checked: bool) -> String {
    match (checked, n, sec) {
        (false, ..) => "—".into(),
        (true, 0, _) => "0".into(),
        (true, n, s) => format!("{n}/{s}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_progress_bar_redrawn_with_cr_keeps_only_its_last_state() {
        let l = "  [                              ]   0%\r  [###############               ]  50%\r  [##############################] 100%\r";
        assert_eq!(last_redraw(l), "  [##############################] 100%");
        assert_eq!(last_redraw("plain line"), "plain line");
        assert_eq!(last_redraw(""), "");
        assert_eq!(last_redraw("a\r\r"), "a");
    }

    #[test]
    fn clean_strips_terminal_escapes() {
        let c = clean("ok \x1b[2J\x1b]0;pwned\x07 apt\t\u{9b}x\u{7f}");
        assert!(!c.chars().any(|ch| ch.is_control()), "{c:?}");
        assert_eq!(clean("café ñ"), "café ñ");
    }

    #[test]
    fn trunc_cleans() {
        assert!(!trunc("a\x1b]0;x\x07b", 40).contains('\x1b'));
    }

    #[test]
    fn masks_url_credentials() {
        assert_eq!(mask_url_creds("get https://u:p@repo.x/a b"), "get https://***@repo.x/a b");
        assert_eq!(mask_url_creds("https://repo.x/a@b"), "https://repo.x/a@b");
        assert_eq!(mask_url_creds("no urls"), "no urls");
    }
}
