use astrcode_client::{
    ClientError, ClientErrorKind, ConversationBannerErrorCodeDto, ConversationErrorEnvelopeDto,
};

use super::AppController;

impl<T> AppController<T> {
    pub(super) fn apply_status_error(&mut self, error: ClientError) {
        self.state.set_error_status(error.message);
    }

    pub(super) fn apply_hydration_error(&mut self, error: ClientError) {
        match error.kind {
            ClientErrorKind::AuthExpired
            | ClientErrorKind::CursorExpired
            | ClientErrorKind::StreamDisconnected
            | ClientErrorKind::TransportUnavailable
            | ClientErrorKind::UnexpectedResponse => self.apply_banner_error(error),
            _ => self.apply_status_error(error),
        }
    }

    pub(super) fn apply_banner_error(&mut self, error: ClientError) {
        self.state.set_banner_error(ConversationErrorEnvelopeDto {
            code: match error.kind {
                ClientErrorKind::AuthExpired => ConversationBannerErrorCodeDto::AuthExpired,
                ClientErrorKind::CursorExpired => ConversationBannerErrorCodeDto::CursorExpired,
                ClientErrorKind::StreamDisconnected
                | ClientErrorKind::TransportUnavailable
                | ClientErrorKind::PermissionDenied
                | ClientErrorKind::Validation
                | ClientErrorKind::NotFound
                | ClientErrorKind::Conflict
                | ClientErrorKind::UnexpectedResponse => {
                    ConversationBannerErrorCodeDto::StreamDisconnected
                },
            },
            message: error.message.clone(),
            rehydrate_required: matches!(error.kind, ClientErrorKind::CursorExpired),
            details: error.details,
        });
        self.state.set_error_status(error.message);
    }
}
