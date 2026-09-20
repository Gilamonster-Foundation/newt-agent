//! One turn's immutable selection from existing profile/persona/plan owners.
//! No scheduler, authority, execution counter, persistence or independent ID.

use serde::Serialize;

use crate::config::{ProfileConfig, ProfilePick, SelfReviewKnobs};

/// The actual selector that requested a technique, retained after deduplication.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TechniqueSource {
    Profile { selection: ProfilePick },
    Persona { name: String },
    Role { name: String },
    PlanStep { id: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SelectedTechnique {
    name: String,
    sources: Vec<TechniqueSource>,
}

/// Configured intent captured before dispatch, not evidence review ran.
/// Serialization belongs inside the existing content-addressed turn receipt.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct CapturedTechniques {
    selections: Vec<SelectedTechnique>,
    #[serde(skip_serializing_if = "Option::is_none")]
    profile: Option<ProfilePick>,
    #[serde(skip_serializing_if = "Option::is_none")]
    self_review: Option<SelfReviewKnobs>,
    /// Actual canonical dispatch context, not an invented explicit selector.
    #[serde(skip_serializing_if = "Option::is_none")]
    plan_step: Option<String>,
}

impl CapturedTechniques {
    /// Combine already-resolved selectors with the one profile's knobs.
    /// This does not resolve a backend, profile name, ambient setting or authority.
    ///
    /// # Errors
    /// Rejects unknown techniques and unmet registry prerequisites.
    pub fn capture(
        profile: Option<(&ProfilePick, &ProfileConfig)>,
        selectors: impl IntoIterator<Item = (TechniqueSource, Vec<String>)>,
    ) -> Result<Self, String> {
        let mut captured = Self::default();
        if let Some((pick, profile)) = profile {
            profile.validate()?;
            captured.profile = Some(pick.clone());
            captured.add(
                TechniqueSource::Profile {
                    selection: pick.clone(),
                },
                &profile.techniques,
            )?;
        }
        for (source, names) in selectors {
            captured.add(source, &names)?;
        }
        // Validate the final composition using the existing profile validator.
        // Selectors carry no per-step knob overrides.
        let composition = ProfileConfig {
            techniques: captured.selections.iter().map(|s| s.name.clone()).collect(),
            ..ProfileConfig::default()
        };
        composition.validate()?;
        if profile.is_some() || captured.enables("self_review") {
            captured.self_review = Some(
                profile.map_or_else(SelfReviewKnobs::default, |(_, profile)| {
                    profile.self_review_knobs()
                }),
            );
        }
        Ok(captured)
    }

    /// Derive a child's final selection from the already captured parent and
    /// its actual role profile/selectors. A role's resolved profile owns its
    /// knobs; otherwise the parent's resolved knobs remain pinned, even when
    /// self_review was inactive in the parent. Context alone never activates it.
    ///
    /// # Errors
    /// Rejects invalid profile/selector composition before child dispatch.
    pub fn for_child(
        &self,
        profile: Option<(&ProfilePick, &ProfileConfig)>,
        selectors: impl IntoIterator<Item = (TechniqueSource, Vec<String>)>,
        plan_step: Option<&str>,
    ) -> Result<Self, String> {
        let mut captured = self.clone();
        if let Some((pick, profile)) = profile {
            profile.validate()?;
            captured.profile = Some(pick.clone());
            captured.self_review = Some(profile.self_review_knobs());
            captured.add(
                TechniqueSource::Profile {
                    selection: pick.clone(),
                },
                &profile.techniques,
            )?;
        }
        for (source, names) in selectors {
            captured.add(source, &names)?;
        }
        ProfileConfig {
            techniques: captured.selections.iter().map(|s| s.name.clone()).collect(),
            ..ProfileConfig::default()
        }
        .validate()?;
        if captured.enables("self_review") && captured.self_review.is_none() {
            captured.self_review = Some(SelfReviewKnobs::default());
        }
        captured.plan_step = plan_step.map(str::to_owned);
        Ok(captured)
    }

    /// Configuration can be inherited by a later selector even when inactive.
    #[must_use]
    pub fn has_context(&self) -> bool {
        !self.is_empty() || self.self_review.is_some() || self.plan_step.is_some()
    }

    #[must_use]
    pub fn plan_step(&self) -> Option<&str> {
        self.plan_step.as_deref()
    }

    #[must_use]
    pub fn profile(&self) -> Option<&ProfilePick> {
        self.profile.as_ref()
    }

    fn add(&mut self, source: TechniqueSource, names: &[String]) -> Result<(), String> {
        crate::kit::validate_selection(names)?;
        for name in names {
            if let Some(existing) = self.selections.iter_mut().find(|s| s.name == *name) {
                if !existing.sources.contains(&source) {
                    existing.sources.push(source.clone());
                }
            } else {
                self.selections.push(SelectedTechnique {
                    name: name.clone(),
                    sources: vec![source.clone()],
                });
            }
        }
        Ok(())
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.selections.is_empty()
    }

    #[must_use]
    pub fn enables(&self, name: &str) -> bool {
        self.selections
            .iter()
            .any(|selection| selection.name == name)
    }

    #[must_use]
    pub fn self_review(&self) -> Option<SelfReviewKnobs> {
        self.enables("self_review")
            .then_some(self.self_review.unwrap_or_default())
    }

    #[must_use]
    pub fn sources(&self, name: &str) -> &[TechniqueSource] {
        self.selections
            .iter()
            .find(|selection| selection.name == name)
            .map_or(&[], |selection| selection.sources.as_slice())
    }
}

#[cfg(test)]
#[path = "kit_selection_tests.rs"]
mod tests;
