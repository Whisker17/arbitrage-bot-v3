/// A verified artifact: the only way consumers can reach a signed payload.
///
/// Fields are private and can only be constructed from within this crate's
/// signing module, after signature, domain, and scope checks have all
/// passed — there is no way to obtain one except through [`super::verify`] or
/// [`super::verify_with_paths`].
#[derive(Debug, Clone)]
pub struct VerifiedArtifact<T> {
    schema_version: String,
    domain: String,
    payload: T,
}

impl<T> VerifiedArtifact<T> {
    pub(crate) fn new(schema_version: String, domain: String, payload: T) -> Self {
        Self {
            schema_version,
            domain,
            payload,
        }
    }

    pub fn schema_version(&self) -> &str {
        &self.schema_version
    }

    pub fn domain(&self) -> &str {
        &self.domain
    }

    pub fn payload(&self) -> &T {
        &self.payload
    }

    pub fn into_inner(self) -> T {
        self.payload
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposes_fields_via_accessors_only() {
        let artifact = VerifiedArtifact::new("1".to_string(), "example.domain".to_string(), 42u32);
        assert_eq!(artifact.schema_version(), "1");
        assert_eq!(artifact.domain(), "example.domain");
        assert_eq!(*artifact.payload(), 42u32);
        assert_eq!(artifact.into_inner(), 42u32);
    }
}
