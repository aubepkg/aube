from pathlib import Path
root=Path("/tmp/upm-base/crates/aube")
source=(root/"src/commands/install/fetch.rs").read_text()
start=source.index("/// Re-key a canonical-indexed")
end=source.index("\n#[cfg(test)]",start)
(root/"src/commands/install/index_remap.rs").write_text("use std::collections::BTreeMap;\n"+source[start:end])
p=root/"examples/index_remap.rs"
p.write_text(p.read_text().replace("remap_indices_to_contextualized(input,", "remap_indices_to_contextualized(&input,"))
