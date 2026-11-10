//! Datos de storage del escenario demo (página Storage): lo que devolverían `pvesh get …/storage[/<s>/content]`, `lvs` y `storage.cfg`.
//! Es determinista y coherente con los discos de los CT del escenario: los números salen de los volúmenes recibidos.
use std::collections::BTreeMap;

const GIB: u64 = 1 << 30;
const DAY: u64 = 86_400;

/// Un disco de un CT del escenario (`volid` = `storage:volumen`).
pub struct Vol {
    pub id: u32,
    pub volid: String,
    pub size: u64,
    pub running: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Fixture {
    pub list: String,
    /// `pvesh get /nodes/<n>/lxc` y `/qemu`: `disk`/`maxdisk` por guest (lo que muestra la GUI de Proxmox).
    pub lxc: String,
    pub qemu: String,
    pub content: BTreeMap<String, String>,
    pub lvs: String,
    pub cfg: String,
}

/// % usado de verdad (sistema de archivos), repartido de forma estable por id. El 131 (caso arr) está al 66%; el 133 al 99%.
fn use_pct(id: u32) -> u64 {
    match id {
        131 => 66,
        133 => 99,
        _ => 25 + (id as u64 * 7) % 50,
    }
}

/// % de bloques del thin pool que ocupa el LV: igual al uso real, salvo el 131 (borró archivos sin hacer trim).
fn thin_pct(id: u32) -> f64 {
    if id == 131 {
        99.8
    } else {
        use_pct(id) as f64
    }
}

fn row(name: &str, kind: &str, content: &str, total: u64, used: u64) -> serde_json::Value {
    serde_json::json!({"storage": name, "type": kind, "content": content, "active": 1, "enabled": 1, "shared": 0, "total": total, "used": used, "avail": total.saturating_sub(used), "used_fraction": used as f64 / total as f64})
}

/// Mediodía UTC de hace `d` días: la fecha local no cambia con la zona horaria de quien corre los tests.
fn noon(now: u64, d: u64) -> u64 {
    (now / DAY - d) * DAY + 12 * 3600
}

pub fn demo(vols: &[Vol], now: u64) -> Fixture {
    let st = |v: &Vol| v.volid.split(':').next().unwrap_or("").to_string();
    let used_of = |v: &Vol| v.size * use_pct(v.id) / 100;
    let mut ids: Vec<u32> = vols.iter().map(|v| v.id).collect();
    ids.dedup();
    let root = |id: u32| vols.iter().find(|v| v.id == id).map_or(GIB, |v| v.size);

    // --- local-zfs y local-lvm: un volumen por disco
    let mut content: BTreeMap<String, String> = BTreeMap::new();
    for s in ["local-zfs", "local-lvm"] {
        let a: Vec<_> = vols
            .iter()
            .filter(|v| st(v) == s)
            .map(|v| serde_json::json!({"volid": v.volid, "vmid": v.id, "content": "rootdir", "format": if s == "local-zfs" { "subvol" } else { "raw" }, "size": v.size, "used": used_of(v)}))
            .collect();
        content.insert(s.into(), serde_json::Value::Array(a).to_string());
    }
    let zfs_used: u64 = vols.iter().filter(|v| st(v) == "local-zfs").map(used_of).sum::<u64>() + 60 * GIB;

    // --- thin pool (lvs)
    let pool = 40 * GIB;
    let thin: Vec<&Vol> = vols.iter().filter(|v| st(v) == "local-lvm").collect();
    let pool_used: u64 = thin.iter().map(|v| (v.size as f64 * thin_pct(v.id) / 100.0) as u64).sum::<u64>() + 2 * GIB;
    let lv = |vg: &str, name: &str, attr: &str, size: u64, pool_lv: &str, pct: Option<f64>| serde_json::json!({"vg_name": vg, "lv_name": name, "lv_attr": attr, "lv_size": size.to_string(), "pool_lv": pool_lv, "data_percent": pct.map_or(String::new(), |p| format!("{p:.2}"))});
    let mut lvs = vec![lv("pve", "root", "-wi-ao----", 30 * GIB, "", None), lv("pve", "data", "twi-aotz--", pool, "", Some(pool_used as f64 * 100.0 / pool as f64))];
    for v in &thin {
        let name = v.volid.split(':').nth(1).unwrap_or("");
        lvs.push(lv("pve", name, "Vwi-aotz--", v.size, "data", Some(thin_pct(v.id))));
    }
    let lvs = serde_json::json!({"report": [{"lv": lvs}]}).to_string();

    // --- disk/maxdisk por guest: solo el rootfs (maxdisk ≠ size del .conf, como en Proxmox); un CT apagado informa disk = 0
    let api: Vec<serde_json::Value> = ids
        .iter()
        .filter_map(|id| vols.iter().find(|v| v.id == *id))
        .map(|v| serde_json::json!({"vmid": v.id, "name": format!("ct-{}", v.id), "status": if v.running { "running" } else { "stopped" }, "type": "lxc", "disk": if v.running { used_of(v) / 100 * 97 } else { 0 }, "maxdisk": v.size / 100 * 97}))
        .collect();
    let lxc = serde_json::Value::Array(api).to_string();

    // --- backups: remote-backup-3 (casi todos), local (restos de pxu y algún vzdump suelto)
    let mut pbs = vec![];
    let mut local: Vec<serde_json::Value> = vec![
        serde_json::json!({"volid": "local:vztmpl/debian-12-standard_12.7-1_amd64.tar.zst", "content": "vztmpl", "format": "tzst", "size": 124 * 1024 * 1024u64}),
        serde_json::json!({"volid": "local:vztmpl/alpine-3.20-default_20240908_amd64.tar.xz", "content": "vztmpl", "format": "txz", "size": 3 * 1024 * 1024u64}),
    ];
    let mut pbs_logical = 0u64;
    for &id in &ids {
        let sz = root(id);
        let stamp = |d: u64| {
            let (y, m, dd) = civil(noon(now, d) / DAY);
            format!("{y:04}-{m:02}-{dd:02}T03:0{}:00Z", id % 6)
        };
        let mut pbs_at = |days: u64| {
            let size = sz * 6 / 10;
            pbs_logical += size;
            let t = noon(now, days);
            pbs.push(serde_json::json!({"volid": format!("remote-backup-3:backup/ct/{id}/{}", stamp(days)), "vmid": id, "content": "backup", "format": "pbs-ct", "size": size, "ctime": t}));
        };
        match id {
            100 => {}
            101 => {
                let t = noon(now, 200);
                let size = sz * 3 / 10;
                local.push(serde_json::json!({"volid": format!("local:backup/vzdump-lxc-{id}-2026_04_23-03_00_00.tar.zst"), "vmid": id, "content": "backup", "format": "tar.zst", "size": size, "ctime": t, "notes": format!("ct-{id}")}));
            }
            133 => pbs_at(45),
            103 => {
                pbs_at(2);
                let size = sz * 3 / 10;
                local.push(serde_json::json!({"volid": "local:backup/vzdump-lxc-103-2026_11_07-04_30_00.tar.zst", "vmid": 103, "content": "backup", "format": "tar.zst", "size": size, "ctime": noon(now, 1), "notes": "pxu pre-update ct-103"}));
            }
            _ => {
                pbs_at(1 + id as u64 % 5);
                if id % 3 == 0 {
                    pbs_at(8 + id as u64 % 5);
                }
            }
        }
    }
    let local_used = local.iter().map(|c| c["size"].as_u64().unwrap_or(0)).sum::<u64>() + 18 * GIB;
    content.insert("local".into(), serde_json::Value::Array(local).to_string());
    let pbs_used = pbs_logical * 10 / 42;
    content.insert("remote-backup-3".into(), serde_json::Value::Array(pbs).to_string());

    let list = serde_json::Value::Array(vec![
        row("local", "dir", "iso,vztmpl,backup", 100 * GIB, local_used),
        row("local-lvm", "lvmthin", "rootdir,images", pool, pool_used),
        row("local-zfs", "zfspool", "rootdir,images", 900 * GIB, zfs_used),
        row("remote-backup-3", "pbs", "backup", 2000 * GIB, pbs_used),
    ])
    .to_string();
    let cfg = "dir: local\n\tpath /var/lib/vz\n\tcontent iso,vztmpl,backup\n\nlvmthin: local-lvm\n\tthinpool data\n\tvgname pve\n\tcontent rootdir,images\n\nzfspool: local-zfs\n\tpool rpool/data\n\tcontent rootdir,images\n\npbs: remote-backup-3\n\tdatastore main\n\tserver 192.168.1.50\n\tcontent backup\n".to_string();
    Fixture { list, lxc, qemu: "[]".into(), content, lvs, cfg }
}

/// Días desde 1970-01-01 → (año, mes, día), calendario gregoriano (algoritmo de Hinnant).
fn civil(z: u64) -> (u64, u64, u64) {
    let z = z as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    ((yoe + era * 400 + (m <= 2) as i64) as u64, m as u64, d as u64)
}
