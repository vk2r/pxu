//! A-05: `install.sh` verifica la attestación de GitHub antes de extraer el tarball descargado (curl y gh falsos, sin red).
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

fn stub(dir: &std::path::Path, name: &str, body: &str) {
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Corre `install.sh --release v9.9.9` con un curl que "descarga" archivos válidos de SHA-256 y el `gh` indicado (None = sin gh).
fn run(tag: &str, gh: Option<&str>, skip: bool) -> (bool, String, bool) {
    let base = std::env::temp_dir().join(format!("pxu-ins-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let (bin, root) = (base.join("bin"), base.join("root"));
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    let h = "2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881"; // sha256("x")
    stub(&bin, "curl", &format!(r#"while [ $# -gt 0 ]; do [ "$1" = -o ] && out=$2; shift; done
case "$out" in */SHA256SUMS) echo "{h}  pxu-9.9.9-x86_64-linux.tar.gz" > "$out";; */provenance*) echo '{{}}' > "$out";; *) printf x > "$out";; esac"#));
    if let Some(body) = gh {
        stub(&bin, "gh", body);
    }
    let mut c = Command::new("sh");
    c.arg(concat!(env!("CARGO_MANIFEST_DIR"), "/install.sh")).args(["--release", "v9.9.9"]).env("DESTDIR", &root).env("PATH", format!("{}:/usr/bin:/bin", bin.display())).env_remove("PXU_SKIP_ATTESTATION");
    if skip {
        c.env("PXU_SKIP_ATTESTATION", "1");
    }
    let o = c.output().unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    // "extraído" = el tar falso (contenido "x") llegó a ejecutarse: install.sh lo anuncia tras la verificación, o falla en tar
    let reached_tar = text.contains("build attestation verified") || text.contains("WARNING: PXU_SKIP_ATTESTATION");
    let _ = std::fs::remove_dir_all(&base);
    (o.status.success(), text, reached_tar)
}

#[test]
fn without_gh_the_install_stops_before_extracting() {
    let (ok, text, reached) = run("nogh", None, false);
    assert!(!ok && !reached && text.contains("GitHub CLI (gh) is needed"), "{text}");
}

#[test]
fn an_invalid_attestation_stops_the_install_before_extracting() {
    let (ok, text, reached) = run("bad", Some("exit 1"), false);
    assert!(!ok && !reached && text.contains("attestation of pxu-9.9.9-x86_64-linux.tar.gz is not valid") && text.contains("nothing was installed"), "{text}");
}

#[test]
fn a_valid_attestation_lets_the_install_continue_to_extraction() {
    let (_, text, reached) = run("good", Some(r#"echo "$@" > /dev/null; exit 0"#), false);
    assert!(reached && text.contains("build attestation verified"), "{text}");
}

#[test]
fn skipping_is_explicit_and_loud() {
    let (_, text, reached) = run("skip", None, true);
    assert!(reached && text.contains("WARNING") && text.contains("NOT verified"), "{text}");
}
