//! Dump every object of a bundle as an indented tree; long arrays and byte blobs are elided.
use abgen::unity::bundle_file::{Bundle, FileContent};
use abgen::value::Value;

fn show(v: &Value, name: &str, depth: usize, max_items: usize) {
    let pad = "  ".repeat(depth);
    match v {
        Value::Map(m) => {
            println!("{pad}{name}:");
            for (k, cv) in m.iter() {
                show(cv, k, depth + 1, max_items);
            }
        }
        Value::Array(a) => {
            println!("{pad}{name}: [{} items]", a.len());
            for (i, cv) in a.iter().take(max_items).enumerate() {
                show(cv, &format!("[{i}]"), depth + 1, max_items);
            }
        }
        Value::Bytes(b) => println!("{pad}{name}: <{} bytes>", b.len()),
        Value::Str(s) => println!("{pad}{name}: {:?}", &s[..s.len().min(200)]),
        other => println!("{pad}{name}: {other:?}"),
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("bundle path");
    let max_items: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(3);
    let bundle = Bundle::load_bytes(&std::fs::read(&path).unwrap()).unwrap();
    println!(
        "bundle format={} player={:?} engine={:?}",
        bundle.format_version, bundle.version_player, bundle.version_engine
    );
    for f in &bundle.files {
        match &f.content {
            FileContent::Raw(b) => println!("raw {} {} bytes", f.name, b.len()),
            FileContent::Serialized(sf) => {
                println!("sf {} version={} unity={} platform={} ref_types={} script_types={:?} externals={:?}", f.name, sf.version, sf.unity_version, sf.target_platform, sf.ref_types.len(), sf.script_types,
                    sf.externals.iter().map(|e| (e.path.clone(), e.r#type)).collect::<Vec<_>>());
                for (i, t) in sf.types.iter().enumerate() {
                    println!(
                        "type[{i}] class={} sti={} script_id={:?}",
                        t.class_id,
                        t.script_type_index,
                        t.script_id.map(hex)
                    );
                    // SFDUMP_NODES=<field> prints the type tree under that field.
                    if let (Ok(field), Some(node)) =
                        (std::env::var("SFDUMP_NODES"), t.node.as_ref())
                    {
                        print_nodes(node, &field, false, 0);
                    }
                }
                for obj in &sf.objects {
                    println!(
                        "---- object class={} type_id={} pid={}",
                        obj.class_id, obj.type_id, obj.path_id
                    );
                    match sf.read_typetree(obj) {
                        Ok(v) => show(&v, "root", 0, max_items),
                        Err(e) => println!("  read error {e:#}"),
                    }
                }
            }
        }
    }
}

fn print_nodes(
    n: &abgen::unity::typetree_node::TypeTreeNode,
    field: &str,
    inside: bool,
    depth: usize,
) {
    let inside = inside || n.m_Name == field;
    if inside {
        println!("{}{} {}", "  ".repeat(depth), n.m_Type, n.m_Name);
    }
    for c in &n.m_Children {
        print_nodes(c, field, inside, if inside { depth + 1 } else { 0 });
    }
}

fn hex(b: [u8; 16]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
