use argh::FromArgs;

#[derive(FromArgs, PartialEq, Debug)]
/// An example subcommand. Copy paste this into a new file.
#[argh(subcommand, name = "example")]
pub struct ExampleSubcommand {
    #[argh(switch)]
    /// whether to fooey
    fooey: bool,
}

impl ExampleSubcommand {
    pub async fn main(self) -> anyhow::Result<()> {
        todo!()
    }
}
