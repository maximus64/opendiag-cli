use crate::{
    events::{Cancel, Interaction},
    DiagError, ObdProtocol,
};
pub fn relearn_idle(
    _: &mut ObdProtocol,
    _: &mut dyn Interaction,
    _: &Cancel,
) -> Result<(), DiagError> {
    Err(DiagError::Unsupported(
        "Honda idle relearn is not implemented".into(),
    ))
}
