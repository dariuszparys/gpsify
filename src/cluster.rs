//! Time + folder clustering (K9). Invoked from ingest.
//!
//! 1. Partition by parent directory (`relpath` dirname).
//! 2. Inside a directory:
//!    - dated: rows with a parseable capture time, sorted by that timestamp.
//!      Start a new cluster when the gap to the previous dated photo is > 45 minutes.
//!    - undated: rows with neither `DateTimeOriginal` nor `CreateDate`. One undated
//!      cluster per directory; file mtime is never used.
//! 3. Cluster id = `blake2s(parent + first_path + first_ts_or_"undated")`.

use std::collections::BTreeMap;
use std::path::Path;

use chrono::{Duration, NaiveDateTime};

pub const CLUSTER_GAP_SECS: i64 = 45 * 60;

const DT_FORMATS: &[&str] = &["%Y:%m:%d %H:%M:%S", "%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterMember {
    pub photo_id: String,
    pub relpath: String,
    pub datetime_original: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterPlan {
    pub cluster_id: String,
    pub parent: String,
    pub first_ts: Option<String>,
    pub undated: bool,
    pub members: Vec<ClusterMember>,
}

impl ClusterPlan {
    pub fn n(&self) -> i64 {
        self.members.len() as i64
    }
}

/// Directory of `relpath` in posix form. Album root is `.`.
pub fn parent_of(relpath: &str) -> String {
    let posix = relpath.replace('\\', "/");
    match Path::new(&posix).parent() {
        Some(p) => {
            let s = p.to_string_lossy();
            if s.is_empty() {
                ".".to_string()
            } else {
                s.into_owned()
            }
        }
        None => ".".to_string(),
    }
}

/// Parse EXIF capture time. Does not accept file mtime.
pub fn parse_capture_time(value: Option<&str>) -> Option<NaiveDateTime> {
    let raw = value?.trim();
    if raw.is_empty() {
        return None;
    }
    let mut text = raw;
    if text.ends_with('Z') || text.ends_with('z') {
        text = &text[..text.len() - 1];
    }
    let bytes = text.as_bytes();
    if bytes.len() >= 6
        && (bytes[bytes.len() - 6] == b'+' || bytes[bytes.len() - 6] == b'-')
        && bytes[bytes.len() - 3] == b':'
    {
        text = &text[..text.len() - 6];
    } else if bytes.len() >= 5
        && (bytes[bytes.len() - 5] == b'+' || bytes[bytes.len() - 5] == b'-')
        && text[text.len() - 4..].chars().all(|c| c.is_ascii_digit())
    {
        text = &text[..text.len() - 5];
    }
    let core: String = text.chars().take(19).collect();
    for fmt in DT_FORMATS {
        if let Ok(dt) = NaiveDateTime::parse_from_str(&core, fmt) {
            return Some(dt);
        }
    }
    None
}

/// RFC 7693 BLAKE2s-256 (32-byte digest) matching Python `hashlib.blake2s`.
pub fn blake2s_256(input: &[u8]) -> [u8; 32] {
    const IV: [u32; 8] = [
        0x6A09_E667,
        0xBB67_AE85,
        0x3C6E_F372,
        0xA54F_F53A,
        0x510E_527F,
        0x9B05_688C,
        0x1F83_D9AB,
        0x5BE0_CD19,
    ];
    const SIGMA: [[usize; 16]; 10] = [
        [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
        [14, 10, 4, 8, 9, 15, 13, 6, 1, 12, 0, 2, 11, 7, 5, 3],
        [11, 8, 12, 0, 5, 2, 15, 13, 10, 14, 3, 6, 7, 1, 9, 4],
        [7, 9, 3, 1, 13, 12, 11, 14, 2, 6, 5, 10, 4, 0, 15, 8],
        [9, 0, 5, 7, 2, 4, 10, 15, 14, 1, 11, 12, 6, 8, 3, 13],
        [2, 12, 6, 10, 0, 11, 8, 3, 4, 13, 7, 5, 15, 14, 1, 9],
        [12, 5, 1, 15, 14, 13, 4, 10, 0, 7, 6, 3, 9, 2, 8, 11],
        [13, 11, 7, 14, 12, 1, 3, 9, 5, 0, 15, 4, 8, 6, 2, 10],
        [6, 15, 14, 9, 11, 3, 0, 8, 12, 2, 13, 7, 1, 4, 10, 5],
        [10, 2, 8, 4, 7, 6, 1, 5, 15, 11, 9, 14, 3, 12, 13, 0],
    ];

    #[inline]
    fn g(v: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize, x: u32, y: u32) {
        v[a] = v[a].wrapping_add(v[b]).wrapping_add(x);
        v[d] = (v[d] ^ v[a]).rotate_right(16);
        v[c] = v[c].wrapping_add(v[d]);
        v[b] = (v[b] ^ v[c]).rotate_right(12);
        v[a] = v[a].wrapping_add(v[b]).wrapping_add(y);
        v[d] = (v[d] ^ v[a]).rotate_right(8);
        v[c] = v[c].wrapping_add(v[d]);
        v[b] = (v[b] ^ v[c]).rotate_right(7);
    }

    fn compress(h: &mut [u32; 8], block: &[u8; 64], t: u64, last: bool) {
        let mut m = [0u32; 16];
        for (i, chunk) in block.chunks_exact(4).enumerate() {
            m[i] = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        }
        let mut v = [0u32; 16];
        v[..8].copy_from_slice(h);
        v[8..].copy_from_slice(&IV);
        v[12] ^= t as u32;
        v[13] ^= (t >> 32) as u32;
        if last {
            v[14] = !v[14];
        }
        for s in SIGMA {
            g(&mut v, 0, 4, 8, 12, m[s[0]], m[s[1]]);
            g(&mut v, 1, 5, 9, 13, m[s[2]], m[s[3]]);
            g(&mut v, 2, 6, 10, 14, m[s[4]], m[s[5]]);
            g(&mut v, 3, 7, 11, 15, m[s[6]], m[s[7]]);
            g(&mut v, 0, 5, 10, 15, m[s[8]], m[s[9]]);
            g(&mut v, 1, 6, 11, 12, m[s[10]], m[s[11]]);
            g(&mut v, 2, 7, 8, 13, m[s[12]], m[s[13]]);
            g(&mut v, 3, 4, 9, 14, m[s[14]], m[s[15]]);
        }
        for i in 0..8 {
            h[i] ^= v[i] ^ v[i + 8];
        }
    }

    let mut h = IV;
    h[0] ^= 0x0101_0020;

    let mut offset = 0usize;
    let mut t: u64 = 0;
    while input.len() - offset > 64 {
        let mut block = [0u8; 64];
        block.copy_from_slice(&input[offset..offset + 64]);
        t += 64;
        compress(&mut h, &block, t, false);
        offset += 64;
    }

    let mut last_block = [0u8; 64];
    let rem = input.len() - offset;
    last_block[..rem].copy_from_slice(&input[offset..]);
    t += rem as u64;
    compress(&mut h, &last_block, t, true);

    let mut out = [0u8; 32];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
    out
}

pub fn cluster_id_for(parent: &str, first_path: &str, first_ts: Option<&str>) -> String {
    let ts_part = first_ts.unwrap_or("undated");
    let payload = format!("{parent}{first_path}{ts_part}");
    hex::encode(blake2s_256(payload.as_bytes()))
}

pub fn plan_clusters(members: &[ClusterMember]) -> Vec<ClusterPlan> {
    let mut by_parent: BTreeMap<String, Vec<ClusterMember>> = BTreeMap::new();
    for m in members {
        by_parent.entry(parent_of(&m.relpath)).or_default().push(m.clone());
    }

    let mut plans = Vec::new();
    for (parent, group) in by_parent {
        let mut dated = Vec::new();
        let mut undated = Vec::new();
        for m in group {
            if parse_capture_time(m.datetime_original.as_deref()).is_none() {
                undated.push(m);
            } else {
                dated.push(m);
            }
        }
        dated.sort_by(|a, b| {
            let ta = parse_capture_time(a.datetime_original.as_deref());
            let tb = parse_capture_time(b.datetime_original.as_deref());
            ta.cmp(&tb).then_with(|| a.relpath.cmp(&b.relpath))
        });
        undated.sort_by(|a, b| a.relpath.cmp(&b.relpath));

        plans.extend(dated_clusters(&parent, &dated));
        if !undated.is_empty() {
            plans.push(build_plan(&parent, undated, true));
        }
    }
    plans
}

fn dated_clusters(parent: &str, dated: &[ClusterMember]) -> Vec<ClusterPlan> {
    if dated.is_empty() {
        return Vec::new();
    }
    let gap = Duration::seconds(CLUSTER_GAP_SECS);
    let mut groups: Vec<Vec<ClusterMember>> = Vec::new();
    let mut current = vec![dated[0].clone()];
    for member in &dated[1..] {
        let prev_t = parse_capture_time(current.last().unwrap().datetime_original.as_deref());
        let this_t = parse_capture_time(member.datetime_original.as_deref());
        match (prev_t, this_t) {
            (Some(p), Some(t)) if t - p <= gap => {
                current.push(member.clone());
            }
            _ => {
                groups.push(current);
                current = vec![member.clone()];
            }
        }
    }
    groups.push(current);
    groups
        .into_iter()
        .map(|g| build_plan(parent, g, false))
        .collect()
}

fn build_plan(parent: &str, members: Vec<ClusterMember>, undated: bool) -> ClusterPlan {
    let first = &members[0];
    let first_ts = if undated {
        None
    } else {
        first.datetime_original.clone()
    };
    let cluster_id = cluster_id_for(parent, &first.relpath, first_ts.as_deref());
    ClusterPlan {
        cluster_id,
        parent: parent.to_string(),
        first_ts,
        undated,
        members,
    }
}
