//! The transforms `publish_string` applies for the standalone reader, the
//! rst parser and the manpage writer (`docutils/transforms/references.py`,
//! `frontmatter.py`, `misc.py`, `universal.py`), in priority order.

use std::collections::HashMap;

use super::nodes::{Id, Tag, Val};
use super::parser::{MsgArgs, P, R};

const SYMBOLS: [&str; 10] = [
    "*", "\u{2020}", "\u{2021}", "\u{a7}", "\u{b6}", "#", "\u{2660}", "\u{2665}", "\u{2666}",
    "\u{2663}",
];

impl P {
    /// `Transformer.apply_transforms`.
    pub(super) fn apply_transforms(&mut self) -> R<()> {
        self.doc.observe_transform = true;
        self.substitutions()?;
        self.propagate_targets();
        self.doc_title();
        self.anonymous_hyperlinks()?;
        self.indirect_hyperlinks()?;
        self.footnotes()?;
        self.external_targets();
        self.internal_targets();
        self.transitions()?;
        self.dangling_references()?;
        self.messages();
        self.filter_messages();
        Ok(())
    }

    fn problematic_for(&mut self, msg: Id, node: Id) -> String {
        let msgid = self.doc.set_id(msg);
        let raw = self.doc.n(node).rawsource.clone();
        let prb = self.doc.text_elem(Tag::Problematic, &raw, &raw);
        self.doc.set_str(prb, "refid", &msgid);
        let prbid = self.doc.set_id(prb);
        self.doc.add_backref(msg, &prbid);
        self.doc.replace_self(node, &[prb]);
        prbid
    }

    /// `references.Substitutions`.
    fn substitutions(&mut self) -> R<()> {
        let mut subreflist = self.doc.findall(0, &|t| t == Tag::SubstitutionReference);
        let mut nested: HashMap<String, Vec<String>> = HashMap::new();
        let mut origin: HashMap<Id, Id> = HashMap::new();
        let mut idx = 0;
        while idx < subreflist.len() {
            let r = subreflist[idx];
            idx += 1;
            let refname = self.doc.get_str(r, "refname").unwrap_or_default();
            let key = if self.doc.substitution_defs.contains_key(&refname) {
                Some(refname.clone())
            } else {
                self.doc
                    .substitution_names
                    .get(&refname.to_lowercase())
                    .cloned()
            };
            let Some(key) = key else {
                let msg = self.error(
                    &format!("Undefined substitution referenced: \"{refname}\"."),
                    MsgArgs::base(r),
                )?;
                self.problematic_for(msg, r);
                continue;
            };
            let subdef = self.doc.substitution_defs[&key];
            if self.doc.astext(subdef).chars().count() > 50_000 {
                let msg = self.error(
                    &format!("Substitution definition \"{key}\" exceeds the line-length-limit."),
                    MsgArgs::default(),
                )?;
                self.problematic_for(msg, r);
                continue;
            }
            let parent = self.doc.parent(r).unwrap();
            let index = self.doc.index(parent, r);
            let atts = &self.doc.n(subdef).attrs;
            let (ltrim, rtrim) = (
                atts.contains_key("ltrim") || atts.contains_key("trim"),
                atts.contains_key("rtrim") || atts.contains_key("trim"),
            );
            if ltrim && index > 0 {
                let prev = self.doc.children(parent)[index - 1];
                if self.doc.tag(prev) == Tag::Text {
                    let t = super::chars::rstrip(&self.doc.n(prev).text).to_string();
                    let n = self.doc.text(&t);
                    self.doc.nm(parent).children[index - 1] = n;
                    self.doc.nm(n).parent = Some(parent);
                }
            }
            if rtrim && self.doc.children(parent).len() > index + 1 {
                let nx = self.doc.children(parent)[index + 1];
                if self.doc.tag(nx) == Tag::Text {
                    let t = super::chars::lstrip(&self.doc.n(nx).text).to_string();
                    let n = self.doc.text(&t);
                    self.doc.nm(parent).children[index + 1] = n;
                    self.doc.nm(n).parent = Some(parent);
                }
            }
            let copy = self.doc.deepcopy(subdef);
            let mut circular = false;
            for nref in self.doc.findall(copy, &|t| t == Tag::SubstitutionReference) {
                let nname = self
                    .doc
                    .get_str(nref, "refname")
                    .unwrap_or_default()
                    .to_lowercase();
                let nested_name = self
                    .doc
                    .substitution_names
                    .get(&nname)
                    .cloned()
                    .unwrap_or_default();
                let list = nested.entry(nested_name.clone()).or_default();
                if list.contains(&nested_name) {
                    circular = true;
                    break;
                }
                list.push(key.clone());
                origin.insert(nref, r);
                subreflist.push(nref);
            }
            if circular {
                let parent = self.doc.parent(r).unwrap();
                if self.doc.tag(parent) == Tag::SubstitutionDefinition {
                    let raw = self.doc.n(parent).rawsource.clone();
                    let lb = self.literal(&raw);
                    let line = self.doc.n(parent).line.map(|l| l as isize);
                    let msg = self.error(
                        "Circular substitution definition detected:",
                        MsgArgs {
                            line,
                            base_node: Some(parent),
                            children: vec![lb],
                            ..Default::default()
                        },
                    )?;
                    self.doc.replace_self(parent, &[msg]);
                } else {
                    let mut o = r;
                    while let Some(&p) = origin.get(&o) {
                        o = p;
                    }
                    let msg = self.error(
                        &format!("Circular substitution definition referenced: \"{refname}\"."),
                        MsgArgs::base(o),
                    )?;
                    self.problematic_for(msg, r);
                }
                continue;
            }
            let children = self.doc.children(copy).to_vec();
            self.doc.replace_self(r, &children);
            for c in children {
                if self.doc.tag(c).is_referential() && self.doc.has(c, "refname") {
                    self.doc.note_refname(c);
                }
            }
        }
        Ok(())
    }

    /// `references.PropagateTargets`.
    fn propagate_targets(&mut self) {
        for target in self.doc.findall(0, &|t| t == Tag::Target) {
            let parent = self.doc.parent(target);
            if parent.is_some_and(|p| self.doc.tag(p).is_text_element())
                || self.doc.has(target, "refid")
                || self.doc.has(target, "refuri")
                || self.doc.has(target, "refname")
            {
                continue;
            }
            let mut next = self.doc.next_node_ascend(target, true);
            while let Some(n) = next {
                if self.doc.tag(n) != Tag::SystemMessage {
                    break;
                }
                next = self.doc.next_node_ascend(n, false);
            }
            let Some(next) = next else { continue };
            let t = self.doc.tag(next);
            if (t.is_invisible() || t.is_targetable()) && t != Tag::Target {
                continue;
            }
            let ids = self.doc.list(target, "ids");
            let names = self.doc.list(target, "names");
            for id in &ids {
                self.doc.push_list(next, "ids", id);
                self.doc.ids.insert(id.clone(), next);
            }
            for n in &names {
                self.doc.push_list(next, "names", n);
            }
            if let Some(first) = ids.first() {
                self.doc.set_str(target, "refid", first);
            }
            self.doc.set(target, "ids", Val::List(Vec::new()));
            self.doc.set(target, "names", Val::List(Vec::new()));
            self.doc.note_refid(target);
        }
    }

    /// `frontmatter.DocTitle`.
    fn doc_title(&mut self) {
        if let Some((section, index)) = self.candidate_index(0) {
            for att in ["ids", "classes", "names", "dupnames"] {
                for v in self.doc.list(section, att) {
                    if !self.doc.list(0, att).contains(&v) {
                        self.doc.push_list(0, att, &v);
                    }
                }
            }
            let sch = self.doc.children(section).to_vec();
            let dch = self.doc.children(0).to_vec();
            let mut new = vec![sch[0]];
            new.extend_from_slice(&dch[..index]);
            new.extend_from_slice(&sch[1..]);
            self.doc.set_children(0, new);
            if let Some((sub, index)) = self.candidate_index(0) {
                let subtitle = self.doc.elem(Tag::Subtitle, "");
                for att in ["ids", "classes", "names", "dupnames"] {
                    for v in self.doc.list(sub, att) {
                        self.doc.push_list(subtitle, att, &v);
                    }
                }
                let title_children = self.doc.children(self.doc.children(sub)[0]).to_vec();
                self.doc.set_children(subtitle, title_children);
                let dch = self.doc.children(0).to_vec();
                let sch = self.doc.children(sub).to_vec();
                let mut new = vec![dch[0], subtitle];
                new.extend_from_slice(&dch[1..index]);
                new.extend_from_slice(&sch[1..]);
                self.doc.set_children(0, new);
            }
        }
    }

    /// `TitlePromoter.candidate_index`.
    fn candidate_index(&self, node: Id) -> Option<(Id, usize)> {
        let index = self
            .doc
            .first_child_not_matching(node, &|t| t.is_prebibliographic())?;
        let ch = self.doc.children(node);
        if ch.len() > index + 1 || self.doc.tag(ch[index]) != Tag::Section {
            return None;
        }
        Some((ch[index], index))
    }

    /// `references.AnonymousHyperlinks`.
    fn anonymous_hyperlinks(&mut self) -> R<()> {
        let refs: Vec<Id> = self
            .doc
            .findall(0, &|t| t == Tag::Reference)
            .into_iter()
            .filter(|&n| self.doc.truthy(n, "anonymous"))
            .collect();
        let targets: Vec<Id> = self
            .doc
            .findall(0, &|t| t == Tag::Target)
            .into_iter()
            .filter(|&n| self.doc.truthy(n, "anonymous"))
            .collect();
        if refs.len() != targets.len() {
            let msg = self.error(
                &format!(
                    "Anonymous hyperlink mismatch: {} references but {} targets.\nSee \"backrefs\" attribute for IDs.",
                    refs.len(),
                    targets.len()
                ),
                MsgArgs::default(),
            )?;
            for r in refs {
                self.problematic_for(msg, r);
            }
            return Ok(());
        }
        for (r, mut target) in refs.into_iter().zip(targets) {
            loop {
                if let Some(uri) = self.doc.get_str(target, "refuri") {
                    self.doc.set_str(r, "refuri", &uri);
                    self.doc.nm(r).resolved = true;
                    break;
                }
                let ids = self.doc.list(target, "ids");
                if ids.is_empty() {
                    let refid = self.doc.get_str(target, "refid").unwrap_or_default();
                    target = self.doc.ids[&refid];
                    continue;
                }
                self.doc.set_str(r, "refid", &ids[0]);
                self.doc.note_refid(r);
                break;
            }
        }
        Ok(())
    }

    /// `references.IndirectHyperlinks`.
    fn indirect_hyperlinks(&mut self) -> R<()> {
        for target in self.doc.indirect_targets.clone() {
            if !self.doc.n(target).resolved {
                self.resolve_indirect_target(target)?;
            }
            self.resolve_indirect_references(target);
        }
        Ok(())
    }

    fn resolve_indirect_target(&mut self, target: Id) -> R<()> {
        let refname = self.doc.get_str(target, "refname");
        let reftarget_id = match &refname {
            None => self.doc.get_str(target, "refid").unwrap_or_default(),
            Some(r) => match self.doc.nameids.get(r).cloned().flatten() {
                Some(id) if !id.is_empty() => id,
                _ => return self.nonexistent_indirect_target(target),
            },
        };
        let reftarget = self.doc.ids[&reftarget_id];
        if self.doc.tag(reftarget) == Tag::Target
            && !self.doc.n(reftarget).resolved
            && self.doc.has(reftarget, "refname")
        {
            if self.doc.n(target).multiply_indirect {
                return self.indirect_target_error(target, "forming a circular reference");
            }
            self.doc.nm(target).multiply_indirect = true;
            self.resolve_indirect_target(reftarget)?;
            self.doc.nm(target).multiply_indirect = false;
        }
        if let Some(uri) = self.doc.get_str(reftarget, "refuri") {
            self.doc.set_str(target, "refuri", &uri);
            self.doc.del(target, "refid");
        } else if let Some(rid) = self.doc.get_str(reftarget, "refid") {
            self.doc.set_str(target, "refid", &rid);
            self.doc.note_refid(target);
        } else if !self.doc.list(reftarget, "ids").is_empty() {
            self.doc.set_str(target, "refid", &reftarget_id);
            self.doc.note_refid(target);
        } else {
            return self.nonexistent_indirect_target(target);
        }
        if refname.is_some() {
            self.doc.del(target, "refname");
        }
        self.doc.nm(target).resolved = true;
        Ok(())
    }

    fn nonexistent_indirect_target(&mut self, target: Id) -> R<()> {
        let refname = self.doc.get_str(target, "refname").unwrap_or_default();
        if self.doc.nameids.contains_key(&refname) {
            self.indirect_target_error(
                target,
                "which is a duplicate, and cannot be used as a unique reference",
            )
        } else {
            self.indirect_target_error(target, "which does not exist")
        }
    }

    fn indirect_target_error(&mut self, target: Id, explanation: &str) -> R<()> {
        let mut naming = String::new();
        let mut reflist = Vec::new();
        let names = self.doc.list(target, "names");
        if let Some(n) = names.first() {
            naming = format!("\"{n}\" ");
        }
        for name in &names {
            reflist.extend(self.doc.refnames.get(name).cloned().unwrap_or_default());
        }
        let ids = self.doc.list(target, "ids");
        for id in &ids {
            reflist.extend(self.doc.refids.get(id).cloned().unwrap_or_default());
        }
        if let Some(i) = ids.first() {
            naming.push_str(&format!("(id=\"{i}\")"));
        }
        let refname = self.doc.get_str(target, "refname").unwrap_or_default();
        let msg = self.error(
            &format!(
                "Indirect hyperlink target {naming}refers to target \"{refname}\", {explanation}."
            ),
            MsgArgs::base(target),
        )?;
        let msgid = self.doc.set_id(msg);
        let mut seen = Vec::new();
        for r in reflist {
            if seen.contains(&r) {
                continue;
            }
            seen.push(r);
            let raw = self.doc.n(r).rawsource.clone();
            let prb = self.doc.text_elem(Tag::Problematic, &raw, &raw);
            self.doc.set_str(prb, "refid", &msgid);
            let prbid = self.doc.set_id(prb);
            self.doc.add_backref(msg, &prbid);
            if self.doc.parent(r).is_some() {
                self.doc.replace_self(r, &[prb]);
            }
        }
        self.doc.nm(target).resolved = true;
        Ok(())
    }

    fn resolve_indirect_references(&mut self, target: Id) {
        let (attname, note) = if self.doc.has(target, "refid") {
            ("refid", true)
        } else if self.doc.has(target, "refuri") {
            ("refuri", false)
        } else {
            return;
        };
        let attval = self.doc.get_str(target, attname).unwrap();
        for name in self.doc.list(target, "names") {
            for r in self.doc.refnames.get(&name).cloned().unwrap_or_default() {
                if self.doc.n(r).resolved {
                    continue;
                }
                self.doc.del(r, "refname");
                self.doc.set_str(r, attname, &attval);
                if note {
                    self.doc.note_refid(r);
                }
                self.doc.nm(r).resolved = true;
                if self.doc.tag(r) == Tag::Target {
                    self.resolve_indirect_references(r);
                }
            }
        }
        for id in self.doc.list(target, "ids") {
            for r in self.doc.refids.get(&id).cloned().unwrap_or_default() {
                if self.doc.n(r).resolved {
                    continue;
                }
                self.doc.del(r, "refid");
                self.doc.set_str(r, attname, &attval);
                if note {
                    self.doc.note_refid(r);
                }
                self.doc.nm(r).resolved = true;
                if self.doc.tag(r) == Tag::Target {
                    self.resolve_indirect_references(r);
                }
            }
        }
    }

    /// `references.Footnotes`.
    fn footnotes(&mut self) -> R<()> {
        let mut labels = Vec::new();
        let mut startnum = self.doc.autofootnote_start;
        for footnote in self.doc.autofootnotes.clone() {
            let label = loop {
                let l = startnum.to_string();
                startnum += 1;
                if !self.doc.nameids.contains_key(&l) {
                    break l;
                }
            };
            let lab = self.doc.text_elem(Tag::Label, "", &label);
            self.doc.insert(footnote, 0, lab);
            for name in self.doc.list(footnote, "names") {
                for r in self
                    .doc
                    .footnote_refs
                    .get(&name)
                    .cloned()
                    .unwrap_or_default()
                {
                    let t = self.doc.text(&label);
                    self.doc.append(r, t);
                    self.doc.del(r, "refname");
                    let fid = self.doc.list(footnote, "ids")[0].clone();
                    self.doc.set_str(r, "refid", &fid);
                    let rid = self.doc.list(r, "ids")[0].clone();
                    self.doc.add_backref(footnote, &rid);
                    self.doc.note_refid(r);
                    self.doc.nm(r).resolved = true;
                }
            }
            if self.doc.list(footnote, "names").is_empty()
                && self.doc.list(footnote, "dupnames").is_empty()
            {
                self.doc.push_list(footnote, "names", &label);
                self.note_explicit_target(footnote, Some(footnote))?;
                labels.push(label);
            }
        }
        self.doc.autofootnote_start = startnum;
        let mut i = 0;
        let refs = self.doc.autofootnote_refs.clone();
        for (k, &r) in refs.iter().enumerate() {
            if self.doc.n(r).resolved || self.doc.has(r, "refid") {
                continue;
            }
            let Some(label) = labels.get(i).cloned() else {
                let msg = self.error(
                    &format!(
                        "Too many autonumbered footnote references: only {} corresponding footnotes available.",
                        labels.len()
                    ),
                    MsgArgs::base(r),
                )?;
                let msgid = self.doc.set_id(msg);
                for &r2 in &refs[k..] {
                    if self.doc.n(r2).resolved || self.doc.has(r2, "refname") {
                        continue;
                    }
                    let raw = self.doc.n(r2).rawsource.clone();
                    let prb = self.doc.text_elem(Tag::Problematic, &raw, &raw);
                    self.doc.set_str(prb, "refid", &msgid);
                    let prbid = self.doc.set_id(prb);
                    self.doc.add_backref(msg, &prbid);
                    self.doc.replace_self(r2, &[prb]);
                }
                break;
            };
            let t = self.doc.text(&label);
            self.doc.append(r, t);
            let id = self.doc.nameids[&label].clone().unwrap_or_default();
            let footnote = self.doc.ids[&id];
            self.doc.set_str(r, "refid", &id);
            self.doc.note_refid(r);
            let rid = self.doc.list(r, "ids")[0].clone();
            self.doc.add_backref(footnote, &rid);
            self.doc.nm(r).resolved = true;
            i += 1;
        }
        let mut symlabels = Vec::new();
        for footnote in self.doc.symbol_footnotes.clone() {
            let n = self.doc.symbol_footnote_start;
            let (reps, index) = (n / SYMBOLS.len(), n % SYMBOLS.len());
            let labeltext = SYMBOLS[index].repeat(reps + 1);
            symlabels.push(labeltext.clone());
            let lab = self.doc.text_elem(Tag::Label, "", &labeltext);
            self.doc.insert(footnote, 0, lab);
            self.doc.symbol_footnote_start += 1;
            self.doc.set_id(footnote);
        }
        let srefs = self.doc.symbol_footnote_refs.clone();
        for (i, &r) in srefs.iter().enumerate() {
            let Some(l) = symlabels.get(i).cloned() else {
                let msg = self.error(
                    &format!(
                        "Too many symbol footnote references: only {} corresponding footnotes available.",
                        symlabels.len()
                    ),
                    MsgArgs::base(r),
                )?;
                let msgid = self.doc.set_id(msg);
                for &r2 in &srefs[i..] {
                    if self.doc.n(r2).resolved || self.doc.has(r2, "refid") {
                        continue;
                    }
                    let raw = self.doc.n(r2).rawsource.clone();
                    let prb = self.doc.text_elem(Tag::Problematic, &raw, &raw);
                    self.doc.set_str(prb, "refid", &msgid);
                    let prbid = self.doc.set_id(prb);
                    self.doc.add_backref(msg, &prbid);
                    self.doc.replace_self(r2, &[prb]);
                }
                break;
            };
            let t = self.doc.text(&l);
            self.doc.append(r, t);
            let footnote = self.doc.symbol_footnotes[i];
            let fid = self.doc.list(footnote, "ids")[0].clone();
            self.doc.set_str(r, "refid", &fid);
            self.doc.note_refid(r);
            let rid = self.doc.list(r, "ids")[0].clone();
            self.doc.add_backref(footnote, &rid);
        }
        for (notes, refs) in [
            (self.doc.footnotes.clone(), self.doc.footnote_refs.clone()),
            (self.doc.citations.clone(), self.doc.citation_refs.clone()),
        ] {
            for note in notes {
                for label in self.doc.list(note, "names") {
                    if let Some(reflist) = refs.get(&label) {
                        let id = self.doc.list(note, "ids")[0].clone();
                        for &r in reflist {
                            if self.doc.n(r).resolved {
                                continue;
                            }
                            self.doc.del(r, "refname");
                            self.doc.set_str(r, "refid", &id);
                            let rid = self.doc.list(r, "ids")[0].clone();
                            self.doc.add_backref(note, &rid);
                            self.doc.nm(r).resolved = true;
                        }
                        self.doc.nm(note).resolved = true;
                    }
                }
            }
        }
        Ok(())
    }

    /// `references.ExternalTargets`.
    fn external_targets(&mut self) {
        for target in self.doc.findall(0, &|t| t == Tag::Target) {
            let Some(refuri) = self.doc.get_str(target, "refuri") else {
                continue;
            };
            for name in self.doc.list(target, "names") {
                for r in self.doc.refnames.get(&name).cloned().unwrap_or_default() {
                    if self.doc.n(r).resolved {
                        continue;
                    }
                    self.doc.del(r, "refname");
                    self.doc.set_str(r, "refuri", &refuri);
                    self.doc.nm(r).resolved = true;
                }
            }
        }
    }

    /// `references.InternalTargets`.
    fn internal_targets(&mut self) {
        for target in self.doc.findall(0, &|t| t == Tag::Target) {
            if self.doc.has(target, "refuri") || self.doc.has(target, "refid") {
                continue;
            }
            for name in self.doc.list(target, "names") {
                let refid = self.doc.nameids.get(&name).cloned().flatten();
                for r in self.doc.refnames.get(&name).cloned().unwrap_or_default() {
                    if self.doc.n(r).resolved {
                        continue;
                    }
                    if let Some(id) = refid.as_ref().filter(|i| !i.is_empty()) {
                        self.doc.del(r, "refname");
                        self.doc.set_str(r, "refid", id);
                    }
                    self.doc.nm(r).resolved = true;
                }
            }
        }
    }

    /// `misc.Transitions`.
    fn transitions(&mut self) -> R<()> {
        for node in self.doc.findall(0, &|t| t == Tag::Transition) {
            let parent = self.doc.parent(node).unwrap();
            let mut index = self.doc.index(parent, node);
            let ch = self.doc.children(parent).to_vec();
            let mut error = None;
            let first_title = self.doc.tag(ch[0]) == Tag::Title;
            if index == 0
                || (first_title
                    && (index == 1
                        || (ch.len() > 1 && self.doc.tag(ch[1]) == Tag::Subtitle && index == 2)))
            {
                let (src, line) = (self.doc.n(node).source.clone(), self.doc.n(node).line);
                error = Some(self.error(
                    "Document or section may not begin with a transition.",
                    MsgArgs {
                        source: src,
                        line: line.map(|l| l as isize),
                        ..Default::default()
                    },
                )?);
            } else if self.doc.tag(ch[index - 1]) == Tag::Transition {
                let (src, line) = (self.doc.n(node).source.clone(), self.doc.n(node).line);
                error = Some(self.error(
                    "At least one body element must separate transitions; adjacent transitions are not allowed.",
                    MsgArgs {
                        source: src,
                        line: line.map(|l| l as isize),
                        ..Default::default()
                    },
                )?);
            }
            if let Some(e) = error {
                self.doc.insert(parent, index, e);
                index += 1;
            }
            if index != self.doc.children(parent).len() - 1 {
                continue;
            }
            let mut sibling = node;

            while index == self.doc.children(self.doc.parent(sibling).unwrap()).len() - 1 {
                sibling = self.doc.parent(sibling).unwrap();
                match self.doc.parent(sibling) {
                    None => {
                        let line = self.doc.n(node).line.map(|l| l as isize);
                        let e = self.error(
                            "Document may not end with a transition.",
                            MsgArgs {
                                line,
                                ..Default::default()
                            },
                        )?;
                        let p = self.doc.parent(node).unwrap();
                        let i = self.doc.index(p, node);
                        self.doc.insert(p, i + 1, e);
                        break;
                    }
                    Some(sp) => {
                        index = self.doc.index(sp, sibling);
                    }
                }
            }
            if self.doc.parent(sibling).is_none() {
                continue;
            }
            let p = self.doc.parent(node).unwrap();
            self.doc.remove(p, node);
            let sp = self.doc.parent(sibling).unwrap();
            self.doc.insert(sp, index + 1, node);
        }
        Ok(())
    }

    /// `references.DanglingReferences` (the visitor part; the unreferenced
    /// target notes are informational).
    fn dangling_references(&mut self) -> R<()> {
        let refs = self.doc.findall(0, &|t| {
            matches!(
                t,
                Tag::Reference | Tag::FootnoteReference | Tag::CitationReference
            )
        });
        for node in refs {
            if self.doc.n(node).resolved || !self.doc.has(node, "refname") {
                continue;
            }
            if !self.attached(node) {
                continue;
            }
            let refname = self.doc.get_str(node, "refname").unwrap();
            match self.doc.nameids.get(&refname).cloned().flatten() {
                None => {
                    let text = if self.doc.nameids.contains_key(&refname) {
                        format!(
                            "Duplicate target name, cannot be used as a unique reference: \"{refname}\"."
                        )
                    } else {
                        format!("Unknown target name: \"{refname}\".")
                    };
                    let msg = self.error(&text, MsgArgs::base(node))?;
                    let msgid = self.doc.set_id(msg);
                    let raw = self.doc.n(node).rawsource.clone();
                    let prb = self.doc.text_elem(Tag::Problematic, &raw, &raw);
                    self.doc.set_str(prb, "refid", &msgid);
                    let prbid = match self.doc.list(node, "ids").first() {
                        Some(i) => i.clone(),
                        None => self.doc.set_id(prb),
                    };
                    self.doc.add_backref(msg, &prbid);
                    self.doc.replace_self(node, &[prb]);
                }
                Some(id) => {
                    self.doc.del(node, "refname");
                    self.doc.set_str(node, "refid", &id);
                    self.doc.nm(node).resolved = true;
                }
            }
        }
        Ok(())
    }

    fn attached(&self, mut id: Id) -> bool {
        loop {
            if id == 0 {
                return true;
            }
            let Some(p) = self.doc.parent(id) else {
                return false;
            };
            if !self.doc.children(p).contains(&id) {
                return false;
            }
            id = p;
        }
    }

    /// `universal.Messages`.
    fn messages(&mut self) {
        let msgs: Vec<Id> = self
            .doc
            .transform_messages
            .iter()
            .copied()
            .filter(|&m| self.doc.parent(m).is_none())
            .collect();
        if !msgs.is_empty() {
            let section = self.doc.elem(Tag::Section, "");
            self.doc.push_list(section, "classes", "system-messages");
            let title = self
                .doc
                .text_elem(Tag::Title, "", "Docutils System Messages");
            self.doc.append(section, title);
            self.doc.extend(section, &msgs);
            self.doc.transform_messages.clear();
            self.doc.append(0, section);
        }
    }

    /// `universal.FilterMessages`.
    fn filter_messages(&mut self) {
        for node in self.doc.findall(0, &|t| t == Tag::SystemMessage) {
            let level = match self.doc.get(node, "level") {
                Some(Val::Int(l)) => *l,
                _ => 0,
            };
            if level < 2 {
                if let Some(p) = self.doc.parent(node)
                    && self.doc.children(p).contains(&node)
                {
                    self.doc.remove(p, node);
                }
                if let Some(id) = self.doc.list(node, "ids").first() {
                    self.doc.ids.remove(id);
                }
            }
        }
        for node in self.doc.findall(0, &|t| t == Tag::Problematic) {
            let refid = self.doc.get_str(node, "refid").unwrap_or_default();
            if !self.doc.ids.contains_key(&refid) {
                let t = self.doc.astext(node);
                let text = self.doc.text(&t);
                let p = self.doc.parent(node).unwrap();
                self.doc.replace(p, node, &[text]);
            }
        }
        for node in self.doc.findall(0, &|t| t == Tag::Section) {
            if self
                .doc
                .list(node, "classes")
                .contains(&"system-messages".to_string())
                && self.doc.children(node).len() == 1
                && let Some(p) = self.doc.parent(node)
            {
                self.doc.remove(p, node);
            }
        }
    }
}
