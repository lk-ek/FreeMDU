use std::{env, fmt::Write, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=read_keys.csv");
    let csv = fs::read_to_string("read_keys.csv").expect("read key registry");
    let mut generated = String::from(
        "/// Known candidates with source attribution.\npub const KNOWN_READ_KEYS: &[ReadKeyCandidate] = &[\n",
    );
    let mut seen = std::collections::BTreeSet::new();
    for line in csv.lines().skip(1) {
        let fields: Vec<_> = line.split(',').collect();
        assert_eq!(fields.len(), 3, "invalid read key registry row");
        let key = u16::from_str_radix(fields[0].strip_prefix("0x").expect("hex key"), 16)
            .expect("16-bit key");
        assert!(seen.insert(key), "duplicate read key");
        writeln!(
            generated,
            "ReadKeyCandidate {{ key: {key}, software_ids: {:?}, source: {:?} }},",
            fields[1], fields[2]
        )
        .expect("format registry");
    }
    generated.push_str("];\n");
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    fs::write(out.join("read_keys.rs"), generated).expect("generate read key registry");
}
