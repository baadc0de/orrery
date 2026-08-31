fn main() {
    if let Err(error) = orrery_ruleset_digest::generate_build_output() {
        panic!("could not derive the D49 ruleset digest: {error}");
    }
}
