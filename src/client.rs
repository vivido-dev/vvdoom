use std::io;

use vivid_sdk::{
    CORE_CONTROL, LIVE_MEDIA, OBSERVABILITY, ProducerAuthentication, ProducerConfig, Session,
    TERMINAL_SURFACE,
};

use crate::cli::Args;

pub fn connect(args: &Args) -> io::Result<Session> {
    let mut required_profiles = vec![
        CORE_CONTROL.into(),
        LIVE_MEDIA.into(),
        OBSERVABILITY.into(),
        TERMINAL_SURFACE.into(),
    ];
    required_profiles.sort();
    let session = Session::connect(ProducerConfig {
        endpoint_control: args.control_endpoint.clone(),
        endpoint_realtime: args.realtime_endpoint.clone(),
        endpoint_bulk: args.bulk_endpoint.clone(),
        authentication: ProducerAuthentication::RootFromEnvironment,
        producer_name: "vvdoom".into(),
        producer_version: env!("CARGO_PKG_VERSION").into(),
        target_profile: TERMINAL_SURFACE.into(),
        required_profiles,
        optional_profiles: Vec::new(),
        ..ProducerConfig::default()
    })?;
    if session.info().target_profile != TERMINAL_SURFACE {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "presenter did not select terminal-surface-v1",
        ));
    }
    Ok(session)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_profiles_are_sorted_and_complete() {
        let mut profiles = [
            CORE_CONTROL.to_owned(),
            LIVE_MEDIA.to_owned(),
            OBSERVABILITY.to_owned(),
            TERMINAL_SURFACE.to_owned(),
        ];
        profiles.sort();
        assert!(profiles.windows(2).all(|pair| pair[0] < pair[1]));
    }
}
