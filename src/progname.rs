use std::sync::OnceLock;

use regex::Regex;

fn message_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?m)(usage: |(?:^|\\n)[ \t]+(?:\x1b\[[0-9;]*m)*)aws([ :])|((?:^|\\n)(?:\x1b\[[0-9;]*m)*)aws(:)")
            .unwrap()
    })
}

fn help_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"(?m)(^[ \t]*(?:\$ )?|``|`|\*|'|")aws( )"#).unwrap())
}

pub fn messages(reference: &str) -> String {
    message_re()
        .replace_all(reference, "${1}${3}awsr${2}${4}")
        .into_owned()
}

pub fn help(rst: &str) -> String {
    help_re().replace_all(rst, "${1}awsr${2}").into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_name_awsr() {
        assert_eq!(
            messages("\naws: [ERROR]: Unable to locate credentials. Run 'aws configure'.\n"),
            "\nawsr: [ERROR]: Unable to locate credentials. Run 'aws configure'.\n"
        );
        assert_eq!(
            messages("usage: aws [options] s3 <subcommand>\n  aws help\n"),
            "usage: awsr [options] s3 <subcommand>\n  awsr help\n"
        );
        assert_eq!(
            messages("(ParamValidation): usage: aws [options]"),
            "(ParamValidation): usage: awsr [options]"
        );
        assert_eq!(
            messages(r#""Message": "x\n\n  aws help\n""#),
            r#""Message": "x\n\n  awsr help\n""#
        );
        assert_eq!(
            messages("\n\x1b[0maws: [ERROR]: x\x1b[0m\n"),
            "\n\x1b[0mawsr: [ERROR]: x\x1b[0m\n"
        );
        assert_eq!(
            messages("arn:aws:iam::aws:policy"),
            "arn:aws:iam::aws:policy"
        );
        assert_eq!(
            messages("new bucket\naws s3 mb s3://bkt\n"),
            "new bucket\naws s3 mb s3://bkt\n"
        );
    }

    #[test]
    fn help_names_awsr() {
        assert_eq!(
            help("::\n\n  aws ec2 describe-regions \\\n    --all-regions\n"),
            "::\n\n  awsr ec2 describe-regions \\\n    --all-regions\n"
        );
        assert_eq!(
            help("Use *aws command help* or ``aws help topics``.\n"),
            "Use *awsr command help* or ``awsr help topics``.\n"
        );
        assert_eq!(
            help("The aws partition, \"aws\", arn:aws:s3, ~/.aws/config.\n"),
            "The aws partition, \"aws\", arn:aws:s3, ~/.aws/config.\n"
        );
    }
}
