use indexmap::IndexMap;

use sidex_types_openapi as openapi;

use nexigon_api::with_actions;

fn main() {
    let mut schemas = serde_json::from_str(include_str!("../schemas.json")).unwrap();

    let mut paths = IndexMap::new();
    macro_rules! add_action {
        ($(($name:literal, $variant:ident, $input:path, $output:path),)*) => {
            $(
                add_action(&mut paths, $name, stringify!($input), stringify!($output), &mut schemas);
            )*
        };
    }

    with_actions!(add_action);

    let components = openapi::Components::new().with_schemas(Some(schemas));

    let openapi = openapi::OpenApi::new(
        "3.0.1".to_owned(),
        openapi::Info::new("Nexigon Hub API".to_owned(), "0.1.0".to_owned()).with_description(
            Some(openapi::Markdown::new(
                include_str!("../docs/description.md").to_owned(),
            )),
        ),
    )
    .with_components(Some(components))
    .with_paths(Some(openapi::Paths::new(paths)))
    .with_tags(Some(
        [
            ("actor", "Actor"),
            ("users", "Users"),
            ("organizations", "Organizations"),
            ("projects", "Projects"),
            ("devices", "Devices"),
            ("repositories", "Repositories"),
            ("instance", "Instance"),
            ("cluster", "Cluster"),
            ("audit", "Audit"),
            ("jobs", "Jobs"),
        ]
        .into_iter()
        .map(|(tag, name)| {
            openapi::Tag::new(tag.to_owned()).with_display_name(Some(name.to_owned()))
        })
        .collect(),
    ));
    serde_json::to_writer_pretty(std::io::stdout(), &openapi).unwrap();
}

pub fn add_action(
    paths: &mut IndexMap<String, openapi::PathItem>,
    name: &str,
    input: &str,
    output: &str,
    schemas: &mut IndexMap<String, openapi::schema::SchemaObject>,
) {
    let input_parts = input.split("::").map(|p| p.trim()).collect::<Vec<_>>();
    let output_parts = output.split("::").map(|p| p.trim()).collect::<Vec<_>>();
    let input_type = input_parts.join(".");
    let output_type = output_parts.join(".");
    let input_type_name = format!("nexigon_api.{input_type}");
    let input_schema = schemas.get_mut(&input_type_name).unwrap();
    let docs = input_schema
        .metadata
        .as_mut()
        .and_then(|m| m.description.take())
        .unwrap_or_default();
    let input_ref = format!("#/components/schemas/nexigon_api.{input_type}");
    let output_ref = format!("#/components/schemas/nexigon_api.{output_type}");
    let path = format!("/api/v1/actions/invoke/{name}");
    paths.insert(
        path,
        openapi::PathItem::new().with_post(Some(
            openapi::Operation::new()
                .with_operation_id(Some(name.to_owned()))
                .with_summary(Some(name.rsplit_once("_").unwrap().1.to_owned()))
                .with_description(Some(openapi::Markdown::new(docs)))
                .with_request_body(Some(openapi::MaybeRef::Value(openapi::RequestBody::new({
                    let mut body = IndexMap::new();
                    body.insert(
                        "application/json".to_string(),
                        openapi::MediaType::new().with_schema(Some(schema_ref(input_ref))),
                    );
                    body
                }))))
                .with_responses(Some(openapi::Responses::new({
                    let mut responses = IndexMap::new();
                    responses.insert(
                        "200".to_string(),
                        openapi::MaybeRef::Value(
                            openapi::Response::new(openapi::Markdown::new("".to_owned()))
                                .with_content({
                                    let mut contents = IndexMap::new();
                                    contents.insert(
                                        "application/json".to_string(),
                                        openapi::MediaType::new()
                                            .with_schema(Some(schema_ref(output_ref))),
                                    );
                                    Some(contents)
                                }),
                        ),
                    );
                    responses
                })))
                .with_tags(Some(vec![name.rsplit_once("_").unwrap().0.to_owned()])),
        )),
    );
}

/// Create a JSON Schema for a reference to another schema.
fn schema_ref(path: impl Into<String>) -> openapi::schema::SchemaObject {
    openapi::schema::SchemaObject::new()
        .with_reference(Some(openapi::schema::SchemaRef::new(path.into())))
}
