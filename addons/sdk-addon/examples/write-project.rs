//! Writes a project with the same function the Create button runs, for a one-off end-to-end
//! check: `cargo run -p sdk-addon --example write-project -- <folder> <name> <author>`.
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [folder, name, author] = args.as_slice() else {
        eprintln!("usage: write-project <folder> <name> <author>");
        std::process::exit(2);
    };
    let form = sdk_addon::project::Form::new(name, author);
    match sdk_addon::project::create(&form.params(), std::path::Path::new(folder)) {
        Ok(project) => println!("{}", project.display()),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}
