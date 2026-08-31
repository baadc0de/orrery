fn main() {
    if let Err(error) = orrery_ruleset_digest::generate_build_output() {
        panic!("could not derive the conformance ruleset digest: {error}");
    }
}
