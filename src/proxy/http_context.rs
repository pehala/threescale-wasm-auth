use log::{debug, error, info};
use proxy_wasm::traits::{Context, HttpContext};
use proxy_wasm::types::FilterHeadersStatus;
use threescalers::application::Application;

use crate::configuration::Configuration;

use super::authrep;
use super::request_headers::RequestHeaders;

pub struct HttpAuthThreescale {
    pub context_id: u32,
    pub configuration: Configuration,
}

impl HttpAuthThreescale {
    //pub const fn configuration(&self) -> &Configuration {
    pub fn configuration(&self) -> &crate::configuration::api::v1::Configuration {
        self.configuration.get()
    }
}

impl HttpContext for HttpAuthThreescale {
    fn on_http_request_headers(&mut self, _: usize) -> FilterHeadersStatus {
        info!("on_http_request_headers: context_id {}", self.context_id);
        //let backend = match self.configuration.get_backend() {
        //    Err(e) => {
        //        error!("error obtaining configuration for 3scale backend: {:?}", e);
        //        return FilterHeadersStatus::Continue;
        //    }
        //    Ok(backend) => backend,
        //};

        let backend = self.configuration().get_backend().ok();

        let rh = RequestHeaders::new(self);

        let ar = match authrep::authrep(self, &rh) {
            Err(e) => {
                error!("error computing authrep {:?}", e);
                self.send_http_response(403, vec![], Some(b"Access forbidden.\n"));
                info!("threescale_wasm_auth: 403 sent");
                return FilterHeadersStatus::StopIteration;
            }
            Ok(params) => params,
        };

        let passthrough_metadata: bool = self.configuration().passthrough_metadata.unwrap_or(false);

        if passthrough_metadata {
            match self.threescale_info_to_metadata(&ar) {
                Ok(()) => return FilterHeadersStatus::Continue,
                Err(e) => {
                    error!("failed to pass app info to next filter: {:?}", e);
                    self.send_http_response(403, vec![], Some(b"Access forbidden.\n"));
                    info!("threescale_wasm_auth: 403 sent");
                    return FilterHeadersStatus::StopIteration;
                }
            }
        }

        if let Some(backend) = backend {
            let request = match authrep::build_call(&ar) {
                Err(e) => {
                    error!("error computing authrep request {:?}", e);
                    self.send_http_response(403, vec![], Some(b"Access forbidden.\n"));
                    info!("threescale_wasm_auth: 403 sent");
                    return FilterHeadersStatus::StopIteration;
                }
                Ok(request) => request,
            };

            // uri will actually just get the whole path + parameters
            let (uri, body) = request.uri_and_body();

            let headers = request
                .headers
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str()))
                .collect::<Vec<_>>();

            let upstream = backend.upstream();
            let call_token = match upstream.call(
                self,
                uri.as_ref(),
                request.method.as_str(),
                headers,
                body.map(str::as_bytes),
                None,
                None,
            ) {
                Ok(call_token) => call_token,
                Err(e) => {
                    error!("on_http_request_headers: could not dispatch HTTP call to {}: did you create the cluster to do so? - {:#?}", upstream.name(), e);
                    self.send_http_response(403, vec![], Some(b"Access forbidden.\n"));
                    info!("threescale_wasm_auth: 403 sent");
                    return FilterHeadersStatus::StopIteration;
                }
            };

            info!(
                "threescale_wasm_auth: on_http_request_headers: call token is {}",
                call_token
            );

            FilterHeadersStatus::StopIteration
        } else {
            // no backend configured
            debug!("on_http_request_headers: no backend configured");
            self.send_http_response(403, vec![], Some(b"Access forbidden.\n"));
            info!("threescale_wasm_auth: 403 sent");
            FilterHeadersStatus::StopIteration
        }
    }

    fn on_http_response_headers(&mut self, _: usize) -> FilterHeadersStatus {
        self.set_http_response_header("Powered-By", Some("3scale"));
        FilterHeadersStatus::Continue
    }
}

impl Context for HttpAuthThreescale {
    fn on_http_call_response(&mut self, call_token: u32, _: usize, _: usize, _: usize) {
        info!(
            "threescale_wasm_auth: http_ctx: on_http_call_response: token id is {}",
            call_token
        );
        let authorized = self
            .get_http_call_response_headers()
            .into_iter()
            .find(|(key, _)| key.as_str() == ":status")
            .map_or(false, |(_, value)| value.as_str() == "200");

        if authorized {
            info!("on_http_call_response: authorized {}", call_token);
            self.resume_http_request();
        } else {
            info!("on_http_call_response: forbidden {}", call_token);
            self.send_http_response(403, vec![], Some(b"Access forbidden.\n"));
            info!("threescale_wasm_auth: 403 sent");
        }
    }
}

impl HttpAuthThreescale {
    fn threescale_info_to_metadata(&self, ar: &authrep::AuthRep) -> Result<(), anyhow::Error> {
        let apps = ar.apps();
        let service = ar.service();
        let usages = ar.usages();

        if apps.is_empty() {
            anyhow::bail!("could not extract application credentials");
        }

        let backend = self.configuration().backend();
        if backend.is_none() {
            anyhow::bail!("backend not configured");
        }
        let upstream = backend.unwrap().upstream();

        if apps.len() > 1 {
            debug!(
                "found more than one source match for application - going to send {:?}",
                apps[0]
            );
        }

        let mut app_id_key = String::new();
        let (header, value) = match &apps[0] {
            Application::AppId(app_id, app_key) => {
                app_id_key.push_str(app_id.as_ref());
                if let Some(key) = app_key {
                    app_id_key.push(':');
                    app_id_key.push_str(key.as_ref());
                }
                ("x-3scale-app-id", app_id_key.as_str())
            }
            Application::UserKey(user_key) => ("x-3scale-user-key", user_key.as_ref()),
            Application::OAuthToken(_token) => anyhow::bail!("Oauth token not supported"),
        };

        // Adding threescale info as request headers
        self.add_http_request_header(header, value);
        self.add_http_request_header("x-3scale-cluster-name", upstream.name());
        self.add_http_request_header("x-3scale-upstream-url", upstream.url.as_str());
        self.add_http_request_header("x-3scale-timeout", &upstream.default_timeout().to_string());
        self.add_http_request_header("x-3scale-service-id", service.id());
        self.add_http_request_header("x-3scale-service-token", service.token());
        self.add_http_request_header("x-3scale-usages", &serde_json::to_string(&usages)?);
        Ok(())
    }
}