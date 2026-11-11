//! FHIRPath for the map engine, from the FHIRPath engine and FHIR model the
//! views use.

use std::sync::Arc;

use fhir_pipes_sof::fhir_model::TableModelProvider;
use fml_engine::{Expressions, Item};
use octofhir_fhirpath::{
    Collection, EvaluationContext, FhirPathEngine, FhirPathValue, ModelProvider,
    create_function_registry,
};

use crate::error::{PipesError, Result};

pub struct FhirPath {
    engine: FhirPathEngine,
    model: Arc<dyn ModelProvider + Send + Sync>,
}

impl FhirPath {
    pub fn new() -> Result<Self> {
        let model: Arc<dyn ModelProvider + Send + Sync> = Arc::new(TableModelProvider);
        let registry = Arc::new(create_function_registry());
        let engine = crate::runtime::block_on(FhirPathEngine::new(registry, model.clone()))
            .map_err(|e| PipesError::Environment(format!("starting the map runtime: {e}")))?
            .map_err(|e| PipesError::Environment(format!("building the FHIRPath engine: {e}")))?;
        Ok(Self { engine, model })
    }

    async fn collection(&self, item: &Item) -> std::result::Result<Collection, String> {
        Collection::from_json_typed_arc(
            Arc::new(item.json.clone()),
            item.type_name.as_deref(),
            Some(self.model.clone()),
        )
        .await
        .map_err(|e| e.to_string())
    }
}

impl Expressions for FhirPath {
    async fn evaluate(
        &self,
        expression: &str,
        focus: Option<&Item>,
        variables: &[(String, Item)],
    ) -> std::result::Result<Vec<Item>, String> {
        let input = match focus {
            Some(item) => self.collection(item).await?,
            None => Collection::empty(),
        };
        let context = EvaluationContext::new(input, self.model.clone(), None, None, None);
        for (name, item) in variables {
            let mut values = self.collection(item).await?.into_vec();
            let value = if values.len() == 1 {
                values.remove(0)
            } else {
                FhirPathValue::collection(values)
            };
            context.set_variable(name.clone(), value);
        }
        let result = self
            .engine
            .evaluate(expression, &context)
            .await
            .map_err(|e| e.to_string())?;
        Ok(result.value.into_vec().into_iter().map(item).collect())
    }
}

/// A FHIRPath value as the engine takes it: JSON and its FHIR type.
fn item(value: FhirPathValue) -> Item {
    let info = value.type_info();
    let type_name = if info.namespace.as_deref() == Some("System") {
        match info.type_name.as_str() {
            "String" => Some("string".to_owned()),
            "Boolean" => Some("boolean".to_owned()),
            "Integer" => Some("integer".to_owned()),
            "Long" => Some("integer64".to_owned()),
            "Decimal" => Some("decimal".to_owned()),
            "Date" => Some("date".to_owned()),
            "DateTime" => Some("dateTime".to_owned()),
            "Time" => Some("time".to_owned()),
            "Quantity" => Some("Quantity".to_owned()),
            _ => None,
        }
    } else {
        info.name.clone().or_else(|| Some(info.type_name.clone()))
    };
    Item {
        json: value.to_json_value(),
        type_name,
    }
}
