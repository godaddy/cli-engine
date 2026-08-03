use clap::Command;

use crate::{
    CliRunOutput, Envelope, Middleware,
    error::exit_code_for_error,
    output::{OutputFormat, render},
    tree::{build_tree_from_clap, render_tree_human},
};

pub(crate) fn render_tree(root: &Command, app_id: &str, middleware: &Middleware) -> CliRunOutput {
    let format: OutputFormat = match middleware.output_format.parse() {
        Ok(format) => format,
        Err(err) => {
            return CliRunOutput {
                exit_code: exit_code_for_error(&err),
                rendered: err.to_string(),
            };
        }
    };
    let tree = build_tree_from_clap(root);
    if format == OutputFormat::Human {
        return CliRunOutput {
            exit_code: 0,
            rendered: render_tree_human(&tree),
        };
    }
    let envelope =
        Envelope::success(tree, app_id.to_owned()).prepare_for_render(&middleware.verbose);
    match render(format, &envelope) {
        Ok(rendered) => CliRunOutput {
            exit_code: 0,
            rendered,
        },
        Err(err) => CliRunOutput {
            exit_code: exit_code_for_error(&err),
            rendered: err.to_string(),
        },
    }
}
