//! Who may read private content. Everything that comes from Gitea counts as
//! private: repos, packages, and the templates, starters and jobs made from
//! them. Every read of private content asks [`Policy::can_read_private`],
//! and content a principal can't read behaves as if it didn't exist.
//!
//! The mode is `GRANARY_PRIVATE_ACCESS` when that is set; otherwise admins
//! choose it on the System page. No mode keeps data of its own, so changing
//! modes only changes the setting. A new mode (for example one that asks
//! Gitea about the user's own permissions) is a new [`Mode`] variant and a
//! new arm in `can_read_private`; the [`Resource`] already says what is
//! being read.

use std::sync::RwLock;

use axum::{extract::FromRequestParts, http::request::Parts};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::{
    AppState,
    auth::{AuthSession, Role, User},
};

/// Who may read private content.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    /// Any signed-in user or valid access token.
    SignedIn,
    /// Admins, plus users and tokens given private access.
    Scoped,
}

impl Mode {
    pub const ALL: [Mode; 2] = [Mode::Scoped, Mode::SignedIn];

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "signed-in" => Some(Self::SignedIn),
            "scoped" => Some(Self::Scoped),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::SignedIn => "signed-in",
            Self::Scoped => "scoped",
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            Self::SignedIn => "Anyone signed in, and any valid access token",
            Self::Scoped => "Admins, plus users and access tokens given private access",
        }
    }
}

/// Where private content comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Gitea,
}

/// What kind of private content.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", tag = "type")]
pub enum Kind {
    Repo,
    Package { format: String },
}

/// A piece of private content: where it comes from, and its owner and name
/// there (a Gitea user or org, and a repo or package name).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resource {
    pub source: Source,
    pub kind: Kind,
    pub owner: String,
    pub name: String,
}

impl Resource {
    pub fn gitea_repo(owner: &str, name: &str) -> Self {
        Self {
            source: Source::Gitea,
            kind: Kind::Repo,
            owner: owner.to_string(),
            name: name.to_string(),
        }
    }
}

/// Who is asking.
#[derive(Clone, Debug)]
pub enum Principal {
    /// No session and no valid token.
    Anonymous,
    /// A signed-in user, or an access token (with the token's own flag).
    User { user: User, token: Option<Token> },
    /// The warmer's standing token. Never reads private content.
    Warmer,
    /// The warmer during one job: it reads what the job's starter may.
    Job { starter: User },
}

/// What an access token itself allows.
#[derive(Clone, Copy, Debug)]
pub struct Token {
    pub private_access: bool,
}

impl Principal {
    /// The principal behind a signed-in session, or anonymous.
    pub fn from_session(auth: &AuthSession) -> Self {
        match &auth.user {
            Some(user) => Principal::User {
                user: user.clone(),
                token: None,
            },
            None => Principal::Anonymous,
        }
    }

    pub fn is_anonymous(&self) -> bool {
        matches!(self, Principal::Anonymous)
    }
}

/// Handlers take the principal the package routes' guard worked out, or
/// the signed-in session's.
impl FromRequestParts<AppState> for Principal {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, Self::Rejection> {
        if let Some(p) = parts.extensions.get::<Principal>() {
            return Ok(p.clone());
        }
        let auth = <AuthSession as FromRequestParts<AppState>>::from_request_parts(parts, st)
            .await
            .ok();
        Ok(auth.map_or(Principal::Anonymous, |a| Principal::from_session(&a)))
    }
}

const SETTING: &str = "private_access";

/// The access policy, with its current mode.
pub struct Policy {
    mode: RwLock<Mode>,
    /// Set by `GRANARY_PRIVATE_ACCESS`, so the System page can't change it.
    locked: bool,
}

impl Policy {
    /// The environment's mode if it sets one, else the saved one, else
    /// `scoped`.
    pub async fn load(db: &SqlitePool, env: Option<Mode>) -> sqlx::Result<Self> {
        let mode = match env {
            Some(m) => m,
            None => crate::auth::setting(db, SETTING)
                .await?
                .and_then(|s| Mode::parse(&s))
                .unwrap_or(Mode::Scoped),
        };
        Ok(Self {
            mode: RwLock::new(mode),
            locked: env.is_some(),
        })
    }

    pub fn mode(&self) -> Mode {
        *self.mode.read().unwrap()
    }

    pub fn locked(&self) -> bool {
        self.locked
    }

    /// Change the mode from the System page.
    pub async fn set_mode(&self, db: &SqlitePool, mode: Mode) -> Result<(), String> {
        if self.locked {
            return Err(
                "GRANARY_PRIVATE_ACCESS is set, so private access can only be changed in the environment"
                    .into(),
            );
        }
        crate::auth::set_setting(db, SETTING, mode.as_str())
            .await
            .map_err(|e| format!("could not save the setting: {e}"))?;
        *self.mode.write().unwrap() = mode;
        Ok(())
    }

    /// Whether `who` may read `what`. The one place this is decided.
    pub fn can_read_private(&self, who: &Principal, what: &Resource) -> bool {
        // Both modes decide by who is asking. A mode that asks Gitea about
        // the user's own permissions would look at `what` here.
        let _ = what;
        self.has_private_access(who)
    }

    /// Whether `who` has private access under the current mode, whatever
    /// the content. For showing or hiding controls in the web UI; reads go
    /// through [`Policy::can_read_private`].
    pub fn has_private_access(&self, who: &Principal) -> bool {
        match who {
            Principal::Anonymous | Principal::Warmer => false,
            Principal::Job { starter } => self.user_may(starter, None),
            Principal::User { user, token } => self.user_may(user, *token),
        }
    }

    /// `can_read_private` for content that may be public (`None`).
    pub fn can_read(&self, who: &Principal, what: Option<&Resource>) -> bool {
        what.is_none_or(|r| self.can_read_private(who, r))
    }

    fn user_may(&self, user: &User, token: Option<Token>) -> bool {
        if user.disabled || user.system {
            return false;
        }
        match self.mode() {
            Mode::SignedIn => true,
            Mode::Scoped => {
                let account = user.role == Role::Admin || user.private_access;
                account && token.is_none_or(|t| t.private_access)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(mode: Mode) -> Policy {
        Policy {
            mode: RwLock::new(mode),
            locked: false,
        }
    }

    fn user(role: Role, private_access: bool) -> Principal {
        Principal::User {
            user: User::for_tests(role, private_access),
            token: None,
        }
    }

    fn token(role: Role, account: bool, token: bool) -> Principal {
        Principal::User {
            user: User::for_tests(role, account),
            token: Some(Token {
                private_access: token,
            }),
        }
    }

    #[test]
    fn modes() {
        let repo = Resource::gitea_repo("acme", "secret");
        let scoped = policy(Mode::Scoped);
        let signed_in = policy(Mode::SignedIn);
        let job = |role, flag| Principal::Job {
            starter: User::for_tests(role, flag),
        };
        // (who, scoped, signed-in)
        let cases = [
            (Principal::Anonymous, false, false),
            (Principal::Warmer, false, false),
            (user(Role::Admin, false), true, true),
            (user(Role::Viewer, false), false, true),
            (user(Role::Viewer, true), true, true),
            (token(Role::Admin, false, false), false, true),
            (token(Role::Admin, false, true), true, true),
            (token(Role::Viewer, false, true), false, true),
            (token(Role::Viewer, true, true), true, true),
            (token(Role::Viewer, true, false), false, true),
            (job(Role::Admin, false), true, true),
            (job(Role::Viewer, false), false, true),
        ];
        for (who, in_scoped, in_signed_in) in cases {
            assert_eq!(
                scoped.can_read_private(&who, &repo),
                in_scoped,
                "scoped: {who:?}"
            );
            assert_eq!(
                signed_in.can_read_private(&who, &repo),
                in_signed_in,
                "signed-in: {who:?}"
            );
            assert!(scoped.can_read(&who, None), "public content: {who:?}");
        }
        // Disabled accounts read nothing private.
        let mut gone = User::for_tests(Role::Admin, true);
        gone.disabled = true;
        let who = Principal::User {
            user: gone,
            token: None,
        };
        assert!(!signed_in.can_read_private(&who, &repo));
        assert_eq!(Mode::parse(" Signed-In "), Some(Mode::SignedIn));
        assert_eq!(Mode::parse("gitea"), None);
    }
}
