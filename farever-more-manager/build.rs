fn main() {
    let config = slint_build::CompilerConfiguration::new().with_style("fluent-light".into());
    slint_build::compile_with_config("ui/addon-manager.slint", config)
        .expect("compile add-on manager UI");
}
