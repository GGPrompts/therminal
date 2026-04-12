fn main() {
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("../../resources/therminal.ico");
        res.compile().expect("failed to compile Windows resources");
    }
    println!("cargo:rerun-if-changed=../../resources/therminal.ico");
    println!("cargo:rerun-if-changed=../../resources/therminal-32.png");
}
