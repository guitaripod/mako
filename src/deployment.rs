use worker::Env;
use crate::error::AppError;

pub struct DeploymentConfig {
    pub mode: DeploymentMode,
}

#[derive(Debug, PartialEq)]
pub enum DeploymentMode {
    Official,
    SelfHosted,
}

impl DeploymentConfig {
    pub fn from_env(env: &Env) -> Result<Self, AppError> {
        let mode_str = env.var("DEPLOYMENT_MODE")
            .map(|v| v.to_string())
            .unwrap_or_else(|_| "self-hosted".to_string());
        
        let mode = match mode_str.as_str() {
            "official" => DeploymentMode::Official,
            "self-hosted" => DeploymentMode::SelfHosted,
            _ => DeploymentMode::SelfHosted,
        };
        
        Ok(DeploymentConfig { mode })
    }
    
    #[allow(dead_code)]
    pub fn is_official(&self) -> bool {
        self.mode == DeploymentMode::Official
    }
}
