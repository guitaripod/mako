use async_trait::async_trait;
use worker::Result;

use super::{provider_for, CostEstimate, ImageProvider, ProviderFeatures, ProviderResponse, UnifiedEditRequest, UnifiedImageRequest};
use crate::edits::ChainLink;
use crate::tenant::TenantPolicy;

/// A tier's models tried in order (migration 035): the next one answers when one is unavailable (billing, outage,
/// a lost reply) or makes no image, so one provider's bad day or stricter filter does not fail the person. A request
/// that is itself wrong (moderation, a broken image) stops at the first link, because the next would refuse it too.
pub struct ChainProvider {
    links: Vec<(String, Box<dyn ImageProvider>)>,
}

impl ChainProvider {
    pub fn new(chain: &[ChainLink], env: &worker::Env, policy: &TenantPolicy) -> Result<Self> {
        let links = chain
            .iter()
            .map(|link| Ok((link.model.clone(), provider_for(&link.model, link.quality.clone(), env, policy)?)))
            .collect::<Result<Vec<_>>>()?;
        Ok(Self { links })
    }

    fn first(&self) -> &dyn ImageProvider {
        self.links[0].1.as_ref()
    }
}

/// Whether a failed link hands the request to the next one: everything but a bad request does.
fn falls_through(error: &worker::Error) -> bool {
    !error.to_string().contains("AppError::BadRequest::")
}

/// Which error a chain that failed everywhere answers with: a decline anywhere says more about the request than an
/// outage elsewhere, so it wins; otherwise the last link's error.
fn pick_error(errors: Vec<worker::Error>) -> worker::Error {
    let declined = errors.iter().position(|e| e.to_string().contains("No images returned by Gemini"));
    let mut errors = errors;
    match declined {
        Some(index) => errors.swap_remove(index),
        None => errors.pop().unwrap_or_else(|| worker::Error::RustError("empty image chain".to_string())),
    }
}

#[async_trait(?Send)]
impl ImageProvider for ChainProvider {
    async fn generate_image(&self, request: &UnifiedImageRequest) -> Result<ProviderResponse> {
        let mut errors = Vec::new();
        for (model, provider) in &self.links {
            match provider.generate_image(request).await {
                Ok(mut response) => {
                    response.served_by = response.served_by.or_else(|| Some(model.clone()));
                    return Ok(response);
                }
                Err(e) if falls_through(&e) => {
                    worker::console_log!("image chain: {} failed, trying the next model: {}", model, e);
                    errors.push(e);
                }
                Err(e) => return Err(e),
            }
        }
        Err(pick_error(errors))
    }

    async fn edit_image(&self, request: &UnifiedEditRequest) -> Result<ProviderResponse> {
        let mut errors = Vec::new();
        for (model, provider) in &self.links {
            match provider.edit_image(request).await {
                Ok(mut response) => {
                    response.served_by = response.served_by.or_else(|| Some(model.clone()));
                    return Ok(response);
                }
                Err(e) if falls_through(&e) => {
                    worker::console_log!("image chain: {} failed on an edit, trying the next model: {}", model, e);
                    errors.push(e);
                }
                Err(e) => return Err(e),
            }
        }
        Err(pick_error(errors))
    }

    fn estimate_cost(&self, request: &UnifiedImageRequest) -> CostEstimate {
        self.first().estimate_cost(request)
    }

    fn estimate_edit_cost(&self, request: &UnifiedEditRequest) -> CostEstimate {
        self.first().estimate_edit_cost(request)
    }

    fn get_supported_features(&self) -> ProviderFeatures {
        let mut features = self.first().get_supported_features();
        features.supports_edit = self.links.iter().any(|(_, p)| p.get_supported_features().supports_edit);
        features
    }

    fn get_name(&self) -> &str {
        self.first().get_name()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::AppError;

    fn error(app: AppError) -> worker::Error {
        app.into()
    }

    #[test]
    fn outages_and_declines_fall_through_and_bad_requests_stop() {
        assert!(falls_through(&error(AppError::InternalError("Gemini unavailable (status 402 code 402)".into()))));
        assert!(falls_through(&error(AppError::InternalError("No images returned by Gemini after 3 attempts (x)".into()))));
        assert!(falls_through(&error(AppError::InternalError("OpenAI API error: status 500 code server_error".into()))));
        assert!(falls_through(&worker::Error::RustError("fetch failed".into())));
        assert!(!falls_through(&error(AppError::BadRequest("Your request was rejected by OpenAI's content policy.".into()))));
        assert!(!falls_through(&error(AppError::BadRequest("Invalid base64 image data".into()))));
    }

    #[test]
    fn a_decline_anywhere_wins_over_an_outage() {
        let picked = pick_error(vec![
            error(AppError::InternalError("No images returned by Gemini after 3 attempts (finishReason=STOP)".into())),
            error(AppError::InternalError("OpenAI API error: status 503 code unknown".into())),
        ]);
        assert!(picked.to_string().contains("No images returned by Gemini"));
        let picked = pick_error(vec![
            error(AppError::InternalError("Gemini unavailable (status 402 code 402)".into())),
            error(AppError::InternalError("OpenAI API error: status 503 code unknown".into())),
        ]);
        assert!(picked.to_string().contains("OpenAI API error: status 503"));
    }
}
