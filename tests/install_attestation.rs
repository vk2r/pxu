//! A-05: `install.sh` verifica la attestación de GitHub antes de extraer el tarball descargado (curl, cosign y apt-get falsos, sin red).
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

fn stub(dir: &std::path::Path, name: &str, body: &str) {
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Herramientas del sistema que `install.sh` usa: el PATH del test solo ve estas (enlazadas), nunca un `cosign`, `apt-get` o `curl` reales del equipo.
const SYSTEM_TOOLS: &[&str] = &[
    "sh", "awk", "basename", "cat", "chmod", "cut", "dirname", "grep", "gzip", "head", "install", "ln", "mkdir", "mktemp", "mv", "readlink", "rm", "sed", "sha256sum", "tail", "tar", "tr", "uname",
];

/// Enlaza en `bin` las herramientas de `SYSTEM_TOOLS` que existan en /usr/bin o /bin.
fn link_system_tools(bin: &std::path::Path) {
    for name in SYSTEM_TOOLS {
        if let Some(src) = ["/usr/bin", "/bin"].iter().map(|d| std::path::Path::new(d).join(name)).find(|p| p.exists()) {
            std::os::unix::fs::symlink(src, bin.join(name)).unwrap();
        }
    }
}

/// Escenario de `run`: qué cosign hay en el PATH, si apt-get puede instalarlo y si el usuario es root.
struct World {
    cosign: Option<&'static str>,
    apt: bool,
    root: bool,
    no_curl: bool,
    skip: bool,
    dpkg: Option<&'static str>,
}

impl World {
    fn new() -> Self {
        World { cosign: None, apt: false, root: true, no_curl: false, skip: false, dpkg: None }
    }
}

/// cosign falso que informa la versión dada y termina con `code` al verificar.
fn cosign_body(version: &str, code: i32) -> String {
    let shown = if version == "devel" { "devel".to_string() } else { format!("v{version}") };
    format!(r#"if [ "$1" = version ]; then echo "GitVersion:    {shown}"; exit 0; fi
echo "$@" >> "$COSIGN_LOG"; [ {code} -ne 0 ] && echo "Error: no matching attestations: identity mismatch" >&2; exit {code}"#)
}

/// Corre `install.sh --release v9.9.9` con un curl que "descarga" archivos válidos de SHA-256 y el mundo indicado.
/// Devuelve (éxito, salida, llegó al tar, líneas que cosign recibió, paquetes que apt instaló).
fn run(tag: &str, w: World) -> (bool, String, bool, String, String) {
    let base = std::env::temp_dir().join(format!("pxu-ins-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let (bin, root) = (base.join("bin"), base.join("root"));
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    link_system_tools(&bin);
    let h = "2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881"; // sha256("x")
    if !w.no_curl {
        stub(&bin, "curl", &format!(r#"while [ $# -gt 0 ]; do [ "$1" = -o ] && out=$2; shift; done
case "$out" in */SHA256SUMS) echo "{h}  pxu-9.9.9-x86_64-linux.tar.gz" > "$out";; */provenance*) echo '{{}}' > "$out";; *) printf x > "$out";; esac"#));
    }
    stub(&bin, "id", if w.root { "echo 0" } else { "echo 1000" });
    if let Some(v) = w.cosign {
        stub(&bin, "cosign", &cosign_body(v, if v.ends_with("-bad") { 1 } else { 0 }));
    }
    if let Some(v) = w.dpkg {
        stub(&bin, "dpkg-query", &format!("echo -n '{v}'"));
    }
    if w.apt {
        // "instala" lo pedido: copia el cosign 2.5.0 (o un curl) preparado en pkg/ al PATH del test
        let pkg = base.join("pkg");
        std::fs::create_dir_all(&pkg).unwrap();
        stub(&pkg, "cosign", &cosign_body("2.5.0", 0));
        stub(&pkg, "curl", "exit 0");
        stub(&bin, "apt-get", &format!(r#"echo "$@" >> "$APT_LOG"
for a in "$@"; do case $a in cosign|curl) cat "{p}/$a" > "{b}/$a"; chmod 755 "{b}/$a";; esac; done
exit 0"#, p = pkg.display(), b = bin.display()));
    }
    let (clog, alog) = (base.join("cosign.log"), base.join("apt.log"));
    let mut c = Command::new("/bin/sh");
    c.arg(concat!(env!("CARGO_MANIFEST_DIR"), "/install.sh")).args(["--release", "v9.9.9"]).env("DESTDIR", &root).env("PATH", &bin).env("COSIGN_LOG", &clog).env("APT_LOG", &alog).env_remove("PXU_SKIP_ATTESTATION");
    if w.skip {
        c.env("PXU_SKIP_ATTESTATION", "1");
    }
    let o = c.output().unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    // "extraído" = el tar falso (contenido "x") llegó a ejecutarse: install.sh lo anuncia tras la verificación, o falla en tar
    let reached_tar = text.contains("build attestation verified") || text.contains("WARNING: PXU_SKIP_ATTESTATION");
    let read = |p: &std::path::Path| std::fs::read_to_string(p).unwrap_or_default();
    let (cl, al) = (read(&clog), read(&alog));
    let _ = std::fs::remove_dir_all(&base);
    (o.status.success(), text, reached_tar, cl, al)
}

#[test]
fn cosign_present_verifies_with_the_release_identity() {
    let (_, text, reached, cl, al) = run("good", World { cosign: Some("2.5.0"), ..World::new() });
    assert!(reached && text.contains("build attestation verified") && al.is_empty(), "{text}");
    assert!(cl.contains("verify-blob-attestation") && cl.contains("--new-bundle-format") && cl.contains("https://token.actions.githubusercontent.com"), "{cl}");
    assert!(cl.contains(r"^https://github.com/vk2r/pxu/\.github/workflows/release\.yml@refs/tags/v"), "{cl}");
}

#[test]
fn a_missing_cosign_is_installed_with_apt_and_then_used() {
    let (_, text, reached, cl, al) = run("apt", World { apt: true, ..World::new() });
    assert!(text.contains("pxu: installing cosign (needed to verify the build attestation)"), "{text}");
    assert!(al.contains("install -y cosign") && reached && cl.contains("verify-blob-attestation"), "{text}\n{al}");
}

#[test]
fn an_old_cosign_is_upgraded_with_apt() {
    let (_, text, reached, _, al) = run("old", World { cosign: Some("2.2.0"), apt: true, ..World::new() });
    assert!(text.contains("older than 2.4.0") && al.contains("install -y cosign") && reached, "{text}");
}

#[test]
fn without_cosign_and_unable_to_install_the_install_stops_with_the_command() {
    for (tag, w) in [("noroot", World { apt: true, root: false, ..World::new() }), ("noapt", World::new())] {
        let (ok, text, reached, _, al) = run(tag, w);
        assert!(!ok && !reached && al.is_empty(), "{text}");
        assert!(text.contains("apt-get install -y cosign") && text.contains("PXU_SKIP_ATTESTATION=1"), "{text}");
    }
}

#[test]
fn a_missing_curl_is_installed_with_apt() {
    let (_, text, _, _, al) = run("curl", World { cosign: Some("2.5.0"), apt: true, no_curl: true, ..World::new() });
    assert!(text.contains("installing curl (needed to download pxu)") && al.contains("install -y curl"), "{text}\n{al}");
}

#[test]
fn an_invalid_attestation_stops_the_install_before_extracting() {
    let (ok, text, reached, _, _) = run("bad", World { cosign: Some("2.5.0-bad"), ..World::new() });
    assert!(!ok && !reached && text.contains("attestation of pxu-9.9.9-x86_64-linux.tar.gz is not valid") && text.contains("nothing was installed"), "{text}");
}

#[test]
fn skipping_is_explicit_and_loud() {
    let (_, text, reached, _, al) = run("skip", World { skip: true, ..World::new() });
    assert!(reached && text.contains("WARNING") && text.contains("NOT verified") && al.is_empty(), "{text}");
}

#[test]
fn debians_cosign_reporting_devel_is_judged_by_its_package_version() {
    let (_, text, reached, cl, al) = run("devel", World { cosign: Some("devel"), dpkg: Some("2.5.0-2+b4"), apt: true, ..World::new() });
    assert!(reached && al.is_empty() && cl.contains("verify-blob-attestation") && !text.contains("installing cosign"), "{text}\n{al}");
    let (_, text, _, _, al) = run("devel-old", World { cosign: Some("devel"), dpkg: Some("1:2.2.0-1"), apt: true, ..World::new() });
    assert!(text.contains("older than 2.4.0") && al.contains("install -y cosign"), "{text}");
}

#[test]
fn a_failed_verification_shows_what_cosign_said() {
    let (ok, text, _, _, _) = run("msg", World { cosign: Some("2.5.0-bad"), ..World::new() });
    assert!(!ok && text.contains("identity mismatch") && text.contains("is not valid"), "{text}");
}
