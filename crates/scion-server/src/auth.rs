//! Authentication at the boundary: every control and object request carries
//! `authorization: Bearer <token>`, checked here once.

use std::time::SystemTime;

use tonic::service::Interceptor;
use tonic::{Request, Status};

use crate::token::TokenKey;

/// Refuses a request whose bearer token is missing, malformed, signed with
/// another key or expired, with UNAUTHENTICATED.
#[derive(Debug, Clone)]
pub(crate) struct Authenticate(pub(crate) TokenKey);

impl Interceptor for Authenticate {
    fn call(&mut self, req: Request<()>) -> Result<Request<()>, Status> {
        let Some(value) = req.metadata().get("authorization") else {
            return Err(refused("no token"));
        };
        let token = value
            .to_str()
            .ok()
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or_else(|| refused("authorization is not `Bearer <token>`"))?;
        match self.0.verify(token.trim(), SystemTime::now()) {
            Ok(_) => Ok(req),
            Err(e) => Err(refused(&e.to_string())),
        }
    }
}

fn refused(why: &str) -> Status {
    Status::unauthenticated(format!(
        "{why}: sign in with `scn login <server>`, or set SCN_TOKEN to a token from \
         `scion-server token`"
    ))
}
