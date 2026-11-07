//! Pins `cuda::CUDNN_LOAD_ORDER` to the import tables of the real cuDNN files: each file comes after
//! every cuDNN file it imports. A file loaded too early does not load, and a partial cuDNN can end
//! the process. Read from the files' headers; nothing is loaded.

use std::path::Path;

use earshot_runtime::cuda::{CUDNN_DLLS, CUDNN_LOAD_ORDER};

/// The names in the import table of the PE file `data` (PE format, "The .idata Section").
fn imports(data: &[u8]) -> Vec<String> {
    let u16_at = |at: usize| u16::from_le_bytes([data[at], data[at + 1]]) as usize;
    let u32_at = |at: usize| {
        u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]]) as usize
    };
    let pe = u32_at(0x3c);
    let sections = u16_at(pe + 6);
    let optional = pe + 24;
    let table = optional + u16_at(pe + 20);
    assert_eq!(u16_at(optional), 0x20b, "a 64-bit image");
    let import_directory = u32_at(optional + 112 + 8);
    let to_offset = |rva: usize| {
        (0..sections)
            .map(|s| table + 40 * s)
            .find_map(|at| {
                let (size, start) = (u32_at(at + 8).max(u32_at(at + 16)), u32_at(at + 12));
                (rva >= start && rva - start < size).then(|| rva - start + u32_at(at + 20))
            })
            .expect("an address in no section")
    };
    let mut names = Vec::new();
    if import_directory == 0 {
        return names;
    }
    let mut at = to_offset(import_directory);
    while u32_at(at + 12) != 0 {
        let name = &data[to_offset(u32_at(at + 12))..];
        let end = name.iter().position(|b| *b == 0).expect("a name's end");
        names.push(String::from_utf8_lossy(&name[..end]).into_owned());
        at += 20;
    }
    names
}

/// `(file, what it imports too early)` for every file of `order` out of order.
fn out_of_order(order: &[&str], imports_of: &dyn Fn(&str) -> Vec<String>) -> Vec<(String, String)> {
    let mut wrong = Vec::new();
    for (i, file) in order.iter().enumerate() {
        for import in imports_of(file) {
            let is_set = order.iter().any(|f| f.eq_ignore_ascii_case(&import));
            let earlier = order[..i].iter().any(|f| f.eq_ignore_ascii_case(&import));
            if is_set && !earlier {
                wrong.push(((*file).to_owned(), import));
            }
        }
    }
    wrong
}

#[test]
#[ignore = "reads NVIDIA's cuDNN files: set EARSHOT_CUDA_DIR to a folder holding them and run cargo test -p earshot-runtime --test cuda_load_order -- --ignored"]
fn cuda_cudnn_load_order_follows_the_real_import_tables() {
    let dir = std::env::var_os("EARSHOT_CUDA_DIR").expect("set EARSHOT_CUDA_DIR");
    let imports_of = |file: &str| {
        let path = Path::new(&dir).join(file);
        let data = std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        imports(&data)
    };
    assert_eq!(out_of_order(&CUDNN_LOAD_ORDER, &imports_of), []);
    // The real tables do constrain the order: the reverse fails.
    let mut reversed = CUDNN_LOAD_ORDER;
    reversed.reverse();
    assert_ne!(out_of_order(&reversed, &imports_of), []);
}

#[test]
fn cuda_cudnn_load_order_holds_every_file_once() {
    let mut order = CUDNN_LOAD_ORDER;
    let mut files = CUDNN_DLLS;
    order.sort_unstable();
    files.sort_unstable();
    assert_eq!(order, files);
}
