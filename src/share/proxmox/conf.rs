//! Parser de `/etc/pve/**/<vmid>.conf` (ARCHITECTURE §1.2).
use serde::Serialize;

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Conf {
    /// Comentarios iniciales (`#...`) decodificados: una entrada por línea.
    pub description: Vec<String>,
    /// Sección principal en orden de aparición; las claves `lxc.*` pueden repetirse.
    pub main: Vec<(String, String)>,
    /// Nombres de secciones `[snapshot]` (sin `pending`).
    pub snapshots: Vec<String>,
    /// Claves de la sección `[pending]`.
    pub pending: Vec<(String, String)>,
}

fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Some(v) = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

impl Conf {
    pub fn parse(text: &str) -> Conf {
        enum Sec {
            Main,
            Pending,
            Snap,
        }
        let mut c = Conf::default();
        let mut sec = Sec::Main;
        let mut leading = true;
        for raw in text.lines() {
            let line = raw.trim_end();
            if line.is_empty() {
                continue;
            }
            if let Some(d) = line.strip_prefix('#') {
                if leading && matches!(sec, Sec::Main) {
                    c.description.push(pct_decode(d.strip_prefix(' ').unwrap_or(d)));
                }
                continue;
            }
            leading = false;
            if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                if name.eq_ignore_ascii_case("pending") {
                    sec = Sec::Pending;
                } else {
                    sec = Sec::Snap;
                    c.snapshots.push(name.to_string());
                }
                continue;
            }
            if let Some((k, v)) = line.split_once(':') {
                let kv = (k.trim().to_string(), v.trim().to_string());
                match sec {
                    Sec::Main => c.main.push(kv),
                    Sec::Pending => c.pending.push(kv),
                    Sec::Snap => {}
                }
            }
        }
        c
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.main.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    pub fn get_u64(&self, key: &str) -> Option<u64> {
        self.get(key).and_then(|v| v.parse().ok())
    }

    pub fn flag(&self, key: &str) -> bool {
        matches!(self.get(key), Some("1"))
    }

    /// Claves cuyo nombre es `prefix` + dígitos (net0, mp1, scsi0…), ordenadas por índice.
    pub fn indexed(&self, prefix: &str) -> Vec<(&str, &str)> {
        let mut v: Vec<(u32, &str, &str)> = self
            .main
            .iter()
            .filter_map(|(k, val)| {
                let n = k.strip_prefix(prefix)?;
                (!n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())).then(|| (n.parse().unwrap_or(0), k.as_str(), val.as_str()))
            })
            .collect();
        v.sort_by_key(|t| t.0);
        v.into_iter().map(|(_, k, val)| (k, val)).collect()
    }

    pub fn raw_lxc(&self) -> Vec<(&str, &str)> {
        self.main.iter().filter(|(k, _)| k.starts_with("lxc.")).map(|(k, v)| (k.as_str(), v.as_str())).collect()
    }
}

/// `"local-zfs:subvol-1-disk-0,size=8G"` → (Some("local-zfs:subvol-1-disk-0"), [("size","8G")]).
/// Un primer campo sin `=` es el valor posicional (volumen / macaddr…).
pub fn parse_props(v: &str) -> (Option<&str>, Vec<(&str, &str)>) {
    let mut positional = None;
    let mut props = Vec::new();
    for (i, part) in v.split(',').enumerate() {
        match part.split_once('=') {
            Some((k, val)) => props.push((k, val)),
            None if i == 0 && !part.is_empty() => positional = Some(part),
            None => {}
        }
    }
    (positional, props)
}

pub fn prop<'a>(props: &[(&'a str, &'a str)], key: &str) -> Option<&'a str> {
    props.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
}

/// "8G" / "512M" / "528K" / "4T" / "1024" (bytes) → bytes.
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let (num, mul) = match s.chars().last()? {
        'K' | 'k' => (&s[..s.len() - 1], 1u64 << 10),
        'M' | 'm' => (&s[..s.len() - 1], 1 << 20),
        'G' | 'g' => (&s[..s.len() - 1], 1 << 30),
        'T' | 't' => (&s[..s.len() - 1], 1 << 40),
        _ => (s, 1),
    };
    let f: f64 = num.parse().ok()?;
    Some((f * mul as f64) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "#Line one%0Aa%20b\n#second\narch: amd64\nlxc.cgroup2.devices.allow: c 10:200 rwm\nlxc.mount.entry: /dev/net dev/net none bind\nnet0: name=eth0,bridge=vmbr0,ip=dhcp\nmp1: s:v1,mp=/b,size=1G\nmp0: s:v0,mp=/a,size=2G\nlock: backup\n\n[snap1]\nmemory: 1\n\n[pending]\nmemory: 2048\n";

    #[test]
    fn sections_description_and_repeated_keys() {
        let c = Conf::parse(SAMPLE);
        assert_eq!(c.description, vec!["Line one\na b", "second"]);
        assert_eq!(c.snapshots, vec!["snap1"]);
        assert_eq!(c.pending, vec![("memory".into(), "2048".into())]);
        assert_eq!(c.get("arch"), Some("amd64"));
        assert_eq!(c.get("memory"), None, "snapshot keys must not leak into main");
        assert_eq!(c.raw_lxc().len(), 2);
        assert_eq!(c.get("lock"), Some("backup"));
        let mps: Vec<_> = c.indexed("mp").iter().map(|(k, _)| *k).collect();
        assert_eq!(mps, vec!["mp0", "mp1"]);
    }

    #[test]
    fn props_and_sizes() {
        let (pos, p) = parse_props("local-zfs:subvol-101-disk-0,size=8G");
        assert_eq!(pos, Some("local-zfs:subvol-101-disk-0"));
        assert_eq!(prop(&p, "size"), Some("8G"));
        let (pos, p) = parse_props("name=eth0,bridge=vmbr0");
        assert_eq!(pos, None);
        assert_eq!(prop(&p, "bridge"), Some("vmbr0"));
        assert_eq!(parse_size("8G"), Some(8 << 30));
        assert_eq!(parse_size("528K"), Some(528 << 10));
        assert_eq!(parse_size("junk"), None);
    }

    #[test]
    fn empty_and_garbage_input() {
        assert_eq!(Conf::parse(""), Conf::default());
        let c = Conf::parse("not a key value\n::\n%zz\n");
        assert!(c.main.iter().all(|(k, _)| k != "not a key value"));
    }
}
