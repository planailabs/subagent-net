#[test]
fn example_spawner_config_parses() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/spawner.toml");
    let c = subnet::spawner::config::Config::load(&path).unwrap();
    assert_eq!(c.types.len(), 2);
}
