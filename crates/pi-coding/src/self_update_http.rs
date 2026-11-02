use std::sync::Arc;

pub use crate::operator_http::OperatorHttpError;
use crate::operator_http::{
    OperatorHttpRequest, OperatorHttpResponse, OperatorHttpTransport, ReleaseChannel,
    ReleaseDownloadUrl,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelfUpdateChannel {
    Stable,
    Prerelease,
}

pub struct SelfUpdateOperatorHttp {
    transport: Arc<OperatorHttpTransport>,
}

impl SelfUpdateOperatorHttp {
    pub fn new() -> Result<Self, OperatorHttpError> {
        Ok(Self {
            transport: OperatorHttpTransport::new()?,
        })
    }

    pub async fn metadata(
        &self,
        channel: SelfUpdateChannel,
    ) -> Result<Box<[u8]>, OperatorHttpError> {
        structured_body(self.transport.execute(metadata_request(channel)).await?)
    }

    /// Download a release asset from its exact `browser_download_url`, which
    /// the caller read from release metadata. The operator transport pins
    /// this URL against the active `RPI_UPDATE_BASE_URL` override (or the fixed
    /// GitHub release download host) so a metadata endpoint can never redirect
    /// an asset fetch to an arbitrary authority.
    pub async fn asset(&self, url: String) -> Result<Box<[u8]>, OperatorHttpError> {
        let response = self.transport.execute(asset_request(url)?).await?;
        match response {
            OperatorHttpResponse::Asset(response) if response.status().is_success() => {
                Ok(response.into_body())
            }
            OperatorHttpResponse::Asset(_) | OperatorHttpResponse::Structured(_) => {
                Err(OperatorHttpError::Protocol)
            }
        }
    }
}

fn metadata_request(channel: SelfUpdateChannel) -> OperatorHttpRequest {
    OperatorHttpRequest::SelfUpdateMetadata {
        channel: match channel {
            SelfUpdateChannel::Stable => ReleaseChannel::Stable,
            SelfUpdateChannel::Prerelease => ReleaseChannel::Prerelease,
        },
    }
}

fn asset_request(url: String) -> Result<OperatorHttpRequest, OperatorHttpError> {
    Ok(OperatorHttpRequest::SelfUpdateAsset {
        url: ReleaseDownloadUrl::parse(url)?,
    })
}

fn structured_body(response: OperatorHttpResponse) -> Result<Box<[u8]>, OperatorHttpError> {
    match response {
        OperatorHttpResponse::Structured(response) if response.status().is_success() => {
            Ok(response.into_body())
        }
        OperatorHttpResponse::Structured(_) | OperatorHttpResponse::Asset(_) => {
            Err(OperatorHttpError::Protocol)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operator_http::OperatorOperation;

    #[test]
    fn metadata_channels_compile_to_the_closed_update_operation() {
        for channel in [SelfUpdateChannel::Stable, SelfUpdateChannel::Prerelease] {
            assert_eq!(
                metadata_request(channel).operation(),
                OperatorOperation::SelfUpdateMetadata
            );
        }
    }

    #[test]
    fn asset_request_accepts_only_structurally_valid_download_urls() {
        let request = asset_request(
            "https://github.com/0x8f701/rpi/releases/download/v1.2.3/rpi-1.2.3-x86_64-unknown-linux-gnu.tar.gz"
                .to_owned(),
        )
        .expect("valid release asset url");
        assert_eq!(request.operation(), OperatorOperation::SelfUpdateAsset);

        // Non-http(s) schemes and empty hosts are rejected structurally before
        // the transport pins the destination.
        assert!(asset_request("ftp://example/asset.tar.gz".to_owned()).is_err());
        assert!(asset_request("not a url".to_owned()).is_err());
    }
}
