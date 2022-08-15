use argh::FromArgs;

#[derive(FromArgs, PartialEq, Debug)]
/// Second subcommand.
#[argh(subcommand, name = "check_config")]
pub struct CheckConfigSubCommand {
    #[argh(switch)]
    /// whether to fooey
    fooey: bool,
}

impl CheckConfigSubCommand {
    pub async fn main(self) -> anyhow::Result<()> {
        todo!()
    }
}
