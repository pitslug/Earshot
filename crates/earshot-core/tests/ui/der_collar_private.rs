use earshot_core::metrics::der::{AMI_COLLAR_PER_SIDE_S, DerConfig};

fn main() {
    // The per-side NIST value written straight into the total-width field: must not compile.
    let _ = DerConfig {
        collar: AMI_COLLAR_PER_SIDE_S,
        uem: None,
    };
}
