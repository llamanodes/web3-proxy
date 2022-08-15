use argh::FromArgs;

#[derive(FromArgs, PartialEq, Debug)]
/// Second subcommand.
#[argh(subcommand, name = "two")]
pub struct SubCommandTwo {
    #[argh(switch)]
    /// whether to fooey
    fooey: bool,
}

impl SubCommandTwo {
    pub async fn main(self) -> anyhow::Result<()> {
        todo!()
    }
}
