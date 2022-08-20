use std::time::{SystemTime, UNIX_EPOCH};

use bcrypt::verify;
use dashmap::DashMap;
use jsonwebtoken::{decode, Algorithm, DecodingKey, Validation};
use lazy_static::lazy_static;
use mongodb::bson::doc;
use reqwest::{
    header::{HeaderMap, HeaderValue, AUTHORIZATION},
    StatusCode,
};
use serde::{Deserialize, Serialize};
use totp_rs::TOTP;
use warp::{
    filters::BoxedFilter,
    header::headers_cloned,
    reply::{Json, WithStatus},
    Filter, Rejection,
};

use crate::{database::user, environment::JWT_SECRET, utilities::generate_id};

use super::login::UserJwt;

#[derive(Deserialize, Serialize)]
pub struct Delete {
    stage: i8,
    continue_token: Option<String>,
    password: Option<String>,
    code: Option<String>,
}

#[derive(Deserialize, Serialize)]
pub struct DeleteError {
    error: String,
}

#[derive(Deserialize, Serialize)]
pub struct DeleteResponse {
    success: Option<bool>,
    continue_token: Option<String>,
}

pub fn route() -> BoxedFilter<(WithStatus<warp::reply::Json>,)> {
    warp::delete()
        .and(
            warp::path!("api" / "user")
                .and(
                    headers_cloned()
                        .map(move |headers: HeaderMap<HeaderValue>| headers)
                        .and_then(authenticate),
                )
                .and(warp::body::json())
                .and_then(handle),
        )
        .boxed()
}

fn jwt_from_header(headers: &HeaderMap<HeaderValue>) -> Option<String> {
    let header = match headers.get(AUTHORIZATION) {
        Some(v) => v,
        None => return None,
    };
    let auth_header = match std::str::from_utf8(header.as_bytes()) {
        Ok(v) => v,
        Err(_) => return None,
    };
    let bearer = "Bearer ";
    if !auth_header.starts_with(bearer) {
        return None;
    }
    Some(auth_header.trim_start_matches(bearer).to_owned())
}

pub async fn authenticate(headers: HeaderMap<HeaderValue>) -> Result<Option<UserJwt>, Rejection> {
    let jwt = jwt_from_header(&headers);
    if let Some(j) = jwt {
        let decoded = decode::<UserJwt>(
            &j,
            &DecodingKey::from_secret(JWT_SECRET.as_ref()),
            &Validation::new(Algorithm::HS512),
        );
        if let Ok(d) = decoded {
            Ok(Some(d.claims))
        } else {
            Ok(None)
        }
    } else {
        Ok(None)
    }
}

pub struct PendingDelete {
    id: String,
    mfa_secret: String,
    time: u64,
}

lazy_static! {
    pub static ref PENDING_DELETES: DashMap<String, PendingDelete> = DashMap::new();
}

pub async fn handle(
    jwt: Option<UserJwt>,
    delete_user: Delete,
) -> Result<WithStatus<Json>, Rejection> {
    if let Some(j) = jwt {
        if delete_user.stage == 1 {
            if let Some(p) = delete_user.password {
                let collection = user::get_collection();
                let user = collection
                    .find_one(
                        doc! {
                            "id": j.id.clone()
                        },
                        None,
                    )
                    .await;
                if let Ok(r) = user {
                    if let Some(u) = r {
                        let verified = verify(p, &u.password_hash)
                            .expect("Unexpected error: failed to verify password");
                        if verified {
                            if u.mfa_enabled {
                                let continue_token = generate_id();
                                let duration = SystemTime::now()
                                    .duration_since(UNIX_EPOCH)
                                    .expect("Unexpected error: time went backwards");
                                let delete_session = PendingDelete {
                                    id: j.id,
                                    mfa_secret: u.mfa_secret.unwrap(),
                                    time: duration.as_secs(),
                                };
                                PENDING_DELETES.insert(continue_token.clone(), delete_session);
                                let response = DeleteResponse {
                                    continue_token: Some(continue_token),
                                    success: None,
                                };
                                Ok(warp::reply::with_status(
                                    warp::reply::json(&response),
                                    StatusCode::OK,
                                ))
                            } else {
                                let result = collection
                                    .delete_one(
                                        doc! {
                                            "id": j.id
                                        },
                                        None,
                                    )
                                    .await;
                                if result.is_ok() {
                                    let error = DeleteResponse {
                                        success: Some(true),
                                        continue_token: None,
                                    };
                                    Ok(warp::reply::with_status(
                                        warp::reply::json(&error),
                                        StatusCode::OK,
                                    ))
                                } else {
                                    let error = DeleteError {
                                        error: "Unable to delete user".to_string(),
                                    };
                                    Ok(warp::reply::with_status(
                                        warp::reply::json(&error),
                                        StatusCode::INTERNAL_SERVER_ERROR,
                                    ))
                                }
                            }
                        } else {
                            let error = DeleteError {
                                error: "Password incorrect".to_string(),
                            };
                            Ok(warp::reply::with_status(
                                warp::reply::json(&error),
                                StatusCode::UNAUTHORIZED,
                            ))
                        }
                    } else {
                        let error = DeleteError {
                            error: "User does not exist".to_string(),
                        };
                        Ok(warp::reply::with_status(
                            warp::reply::json(&error),
                            StatusCode::NOT_FOUND,
                        ))
                    }
                } else {
                    let error = DeleteError {
                        error: "Database error".to_string(),
                    };
                    Ok(warp::reply::with_status(
                        warp::reply::json(&error),
                        StatusCode::INTERNAL_SERVER_ERROR,
                    ))
                }
            } else {
                let error = DeleteError {
                    error: "No password provided".to_string(),
                };
                Ok(warp::reply::with_status(
                    warp::reply::json(&error),
                    StatusCode::BAD_REQUEST,
                ))
            }
        } else if delete_user.stage == 2 {
            if let Some(ct) = delete_user.continue_token {
                if let Some(c) = delete_user.code {
                    let pending_delete = PENDING_DELETES.get(&ct);
                    if let Some(pd) = pending_delete {
                        let duration = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .expect("Unexpected error: time went backwards");
                        if duration.as_secs() - pd.time > 3600 {
                            PENDING_DELETES.remove(&ct);
                            let error = DeleteError {
                                error: "Session expired".to_string(),
                            };
                            Ok(warp::reply::with_status(
                                warp::reply::json(&error),
                                StatusCode::UNAUTHORIZED,
                            ))
                        } else {
                            let temp = Some(pd.mfa_secret.clone());
                            let totp = TOTP::new(
                                totp_rs::Algorithm::SHA256,
                                8,
                                1,
                                30,
                                temp.as_ref().unwrap(),
                                Some("Nextflow Cloud Technologies".to_string()),
                                pd.id.clone(),
                            )
                            .expect("Unexpected error: could not create TOTP instance");
                            let current_code = totp
                                .generate_current()
                                .expect("Unexpected error: failed to generate code");
                            if current_code != c {
                                let error = DeleteError {
                                    error: "Invalid code".to_string(),
                                };
                                Ok(warp::reply::with_status(
                                    warp::reply::json(&error),
                                    StatusCode::UNAUTHORIZED,
                                ))
                            } else {
                                let collection = user::get_collection();
                                let result = collection
                                    .delete_one(
                                        doc! {
                                            "id": j.id
                                        },
                                        None,
                                    )
                                    .await;
                                if result.is_ok() {
                                    let error = DeleteResponse {
                                        success: Some(true),
                                        continue_token: None,
                                    };
                                    Ok(warp::reply::with_status(
                                        warp::reply::json(&error),
                                        StatusCode::OK,
                                    ))
                                } else {
                                    let error = DeleteError {
                                        error: "Unable to delete user".to_string(),
                                    };
                                    Ok(warp::reply::with_status(
                                        warp::reply::json(&error),
                                        StatusCode::INTERNAL_SERVER_ERROR,
                                    ))
                                }
                            }
                        }
                    } else {
                        let error = DeleteError {
                            error: "Session does not exist".to_string(),
                        };
                        Ok(warp::reply::with_status(
                            warp::reply::json(&error),
                            StatusCode::BAD_REQUEST,
                        ))
                    }
                } else {
                    let error = DeleteError {
                        error: "No MFA code".to_string(),
                    };
                    Ok(warp::reply::with_status(
                        warp::reply::json(&error),
                        StatusCode::BAD_REQUEST,
                    ))
                }
            } else {
                let error = DeleteError {
                    error: "No continue token".to_string(),
                };
                Ok(warp::reply::with_status(
                    warp::reply::json(&error),
                    StatusCode::BAD_REQUEST,
                ))
            }
        } else {
            let error = DeleteError {
                error: "Invalid stage".to_string(),
            };
            Ok(warp::reply::with_status(
                warp::reply::json(&error),
                StatusCode::BAD_REQUEST,
            ))
        }
    } else {
        let error = DeleteError {
            error: "Invalid authorization".to_string(),
        };
        Ok(warp::reply::with_status(
            warp::reply::json(&error),
            StatusCode::UNAUTHORIZED,
        ))
    }
}
