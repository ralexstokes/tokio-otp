#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Strategy {
    #[default]
    OneForOne,
    OneForAll,
}
