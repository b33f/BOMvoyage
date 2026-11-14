//! Java manifests: pom.xml (quick-xml pull over `<dependency>` blocks) and
//! Gradle build files (line-level `implementation("g:a:v")` extraction).

use super::super::{Eco, PkgRef, Scope};

fn pkg(group_artifact: &str, version: &str, requirement: Option<String>) -> PkgRef {
    PkgRef {
        name: group_artifact.to_string(),
        version: version.to_string(),
        requirement,
        eco: Eco::Java,
        scope: Scope::Direct,
    }
}

/// pom.xml: `<dependency>` entries with groupId/artifactId/version. Version
/// property references (`${foo}`) are kept as the requirement and warned
/// about by the caller only when unresolvable — here they become the purl's
/// version (honest, if ugly). Parent-managed versions (missing `<version>`)
/// yield a versionless package.
pub fn parse_pom(text: &str) -> Result<Vec<PkgRef>, String> {
    let mut reader = quick_xml::Reader::from_str(text);
    let mut out = Vec::new();
    let mut stack: Vec<String> = Vec::new();
    let mut current: Option<(String, String, String)> = None; // (group, artifact, version)
    let mut in_dependency = false;
    let mut buf = Vec::new();

    loop {
        use quick_xml::events::Event;
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = e.name().as_ref().to_string();
                if name == "dependency" {
                    in_dependency = true;
                    current = Some((String::new(), String::new(), String::new()));
                } else if in_dependency {
                    stack.push(name.clone());
                }
            }
            Ok(Event::Text(t)) => {
                if in_dependency {
                    let text = quick_xml::escape::unescape(&t)
                        .map_err(|e| e.to_string())?
                        .trim()
                        .to_string();
                    if let Some(cur) = current.as_mut() {
                        if let Some(tag) = stack.last() {
                            match tag.as_str() {
                                "groupId" => cur.0 = text,
                                "artifactId" => cur.1 = text,
                                "version" => cur.2 = text,
                                _ => {}
                            }
                        }
                    }
                }
            }
            Ok(Event::End(e)) => {
                let name = e.name().as_ref().to_string();
                if name == "dependency" && in_dependency {
                    in_dependency = false;
                    if let Some((g, a, v)) = current.take() {
                        if !g.is_empty() && !a.is_empty() {
                            let version = v.clone();
                            out.push(pkg(&format!("{g}:{a}"), version.as_str(), Some(v)));
                        }
                    }
                    stack.clear();
                } else {
                    stack.pop();
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(e) => return Err(format!("xml error: {e}")),
        }
        buf.clear();
    }
    Ok(out)
}

/// Gradle: dependencies declared as `implementation("group:artifact:version")`
/// (and api/testImplementation/runtimeOnly/compileOnly variants) or as GAV
/// strings in `dependencies { }` blocks. Version-less GAVs handled.
pub fn parse_gradle(text: &str) -> Result<Vec<PkgRef>, String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        // implementation("g:a:v") / testImplementation 'g:a:v' etc.
        let after_config = CONFIGURATIONS
            .iter()
            .find_map(|c| {
                line.find(c).map(|i| {
                    let rest = &line[i + c.len()..];
                    rest.trim_start_matches(['(', ' '])
                        .trim_start_matches(['"', '\'', ' '])
                })
            })
            .unwrap_or(line);
        let gav = after_config
            .trim_end_matches([')', ' '])
            .trim_end_matches(['"', '\''])
            .trim();
        // A GAV is `group:artifact[:version][:classifier][@ext]` on its own.
        let gav = gav.split_whitespace().next().unwrap_or("");
        let segments: Vec<&str> = gav.split(':').collect();
        if segments.len() < 2 || segments.iter().any(|s| s.is_empty() && segments.len() == 2) {
            continue;
        }
        if segments.iter().any(|s| {
            s.contains(|c: char| !(c.is_alphanumeric() || c == '.' || c == '-' || c == '_'))
        }) {
            continue;
        }
        let group = segments[0];
        let artifact = segments[1];
        let version = segments.get(2).copied().unwrap_or("");
        out.push(pkg(
            &format!("{group}:{artifact}"),
            version,
            (!version.is_empty()).then(|| version.to_string()),
        ));
    }
    Ok(out)
}

const CONFIGURATIONS: &[&str] = &[
    "implementation",
    "api",
    "compileOnly",
    "runtimeOnly",
    "testImplementation",
    "androidTestImplementation",
];

/// gradle.lockfile: TSV `groupId:artifactId:version:configuration[,config…]`
/// then two metadata lines.
pub fn parse_gradle_lockfile(text: &str) -> Result<Vec<PkgRef>, String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split(':');
        let group = parts.next().unwrap_or_default();
        let artifact = parts.next().unwrap_or_default();
        let version = parts.next().unwrap_or_default();
        if group.is_empty() || artifact.is_empty() || version.is_empty() {
            continue; // the lockfile's trailing metadata lines (empty/columns)
        }
        let mut p = pkg(
            &format!("{group}:{artifact}"),
            version,
            Some(version.to_string()),
        );
        p.scope = Scope::Transitive;
        out.push(p);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pom_parses_dependencies() {
        let text = r#"<?xml version="1.0"?>
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <groupId>com.example</groupId>
  <artifactId>app</artifactId>
  <dependencies>
    <dependency>
      <groupId>org.apache.commons</groupId>
      <artifactId>commons-lang3</artifactId>
      <version>3.14.0</version>
    </dependency>
    <dependency>
      <groupId>com.google.guava</groupId>
      <artifactId>guava</artifactId>
    </dependency>
  </dependencies>
</project>"#;
        let pkgs = parse_pom(text).expect("parses");
        assert_eq!(pkgs.len(), 2);
        let commons = pkgs
            .iter()
            .find(|p| p.name == "org.apache.commons:commons-lang3")
            .expect("commons");
        assert_eq!(commons.version, "3.14.0");
        let guava = pkgs
            .iter()
            .find(|p| p.name == "com.google.guava:guava")
            .expect("guava");
        assert!(
            guava.version.is_empty(),
            "parent-managed version yields a versionless package"
        );
    }

    #[test]
    fn gradle_parses_configurations() {
        let text = r#"
dependencies {
    implementation("com.google.guava:guava:33.1.0-jre")
    testImplementation 'junit:junit:4.13.2'
    api "org.slf4j:slf4j-api:2.0.13"
}
"#;
        let pkgs = parse_gradle(text).expect("parses");
        let names: Vec<&str> = pkgs.iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains(&"com.google.guava:guava"));
        assert!(names.contains(&"junit:junit"));
        assert!(names.contains(&"org.slf4j:slf4j-api"));
        let guava = pkgs
            .iter()
            .find(|p| p.name == "com.google.guava:guava")
            .expect("guava");
        assert_eq!(guava.version, "33.1.0-jre");
    }

    #[test]
    fn gradle_lockfile_parses_tsv() {
        let text = "\
# This is a Gradle generated file for dependency locking.
gradleApi()
junit:junit:4.13.2:testCompileClasspath,testCompileRuntimeClasspath
org.slf4j:slf4j-api:2.0.13:runtimeClasspath
empty=
";
        let pkgs = parse_gradle_lockfile(text).expect("parses");
        assert_eq!(pkgs.len(), 2);
        assert_eq!(pkgs[0].name, "junit:junit");
        assert_eq!(pkgs[0].version, "4.13.2");
        assert_eq!(pkgs[0].scope, Scope::Transitive);
    }
}
