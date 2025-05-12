use actix_ws::Session;

use crate::{desk_error::DeskError, model::{signaling::SignalingModel, user::CurrentUser}};

async fn handle_offer(
    session: &mut Session,
    user: &CurrentUser,
    signaling_model: &SignalingModel,
) -> Result<(), DeskError> {
    //
    Ok(())
}