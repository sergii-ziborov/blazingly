use blazingly_docs::{ScaffoldConfig, scaffold};
use std::error::Error;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn Error>> {
    let destination = std::env::args_os()
        .nth(1)
        .map_or_else(|| PathBuf::from("target/generated-scaffold"), PathBuf::from);
    let mut config = ScaffoldConfig::new("hello-blazingly");
    if let Ok(dependency) = std::env::var("BLAZINGLY_SCAFFOLD_DEPENDENCY") {
        config = config.with_dependency(dependency);
    }
    let bundle = scaffold(&config);

    for (relative_path, contents) in bundle.files() {
        let path = destination.join(relative_path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, contents)?;
    }
    println!("{}", destination.display());
    Ok(())
}
