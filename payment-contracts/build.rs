use ethers::contract::MultiAbigen;
use glob::glob;

fn main() {
    // Tell Cargo that if the given file changes, to rerun this build script.
    glob("./abi/*.json").unwrap().for_each(|x| {
        if let Ok(x) = x {
            println!("cargo:rerun-if-changed={:?}", x);
        }
    });

    let gen = MultiAbigen::from_json_files("./abi").unwrap();

    let bindings = gen.build().unwrap();

    bindings.write_to_module("./src/contracts", false).unwrap();
}
