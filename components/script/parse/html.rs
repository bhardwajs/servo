/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use dom::attr::AttrHelpers;
use dom::bindings::codegen::Bindings::NodeBinding::NodeMethods;
use dom::bindings::codegen::InheritTypes::{NodeCast, ElementCast, HTMLScriptElementCast};
use dom::bindings::js::{JS, JSRef, Temporary, OptionalRootable, Root};
use dom::comment::Comment;
use dom::document::{Document, DocumentHelpers};
use dom::documenttype::DocumentType;
use dom::element::{Element, AttributeHandlers, ElementHelpers, ParserCreated};
use dom::htmlscriptelement::HTMLScriptElement;
use dom::htmlscriptelement::HTMLScriptElementHelpers;
use dom::node::{Node, NodeHelpers, TrustedNodeAddress};
use dom::servohtmlparser;
use dom::servohtmlparser::ServoHTMLParser;
use dom::text::Text;
use parse::Parser;

use encoding::all::UTF_8;
use encoding::types::{Encoding, DecodeReplace};

use servo_net::resource_task::{Payload, Done, LoadResponse};
use servo_util::task_state;
use servo_util::task_state::IN_HTML_PARSER;
use std::ascii::AsciiExt;
use std::str::MaybeOwned;
use url::Url;
use html5ever::Attribute;
use html5ever::tree_builder::{TreeSink, QuirksMode, NodeOrText, AppendNode, AppendText};
use string_cache::QualName;

pub enum HTMLInput {
    InputString(String),
    InputUrl(Url),
}

trait SinkHelpers {
    fn get_or_create(&self, child: NodeOrText<TrustedNodeAddress>) -> Temporary<Node>;
}

impl SinkHelpers for servohtmlparser::Sink {
    fn get_or_create(&self, child: NodeOrText<TrustedNodeAddress>) -> Temporary<Node> {
        match child {
            AppendNode(n) => Temporary::new(unsafe { JS::from_trusted_node_address(n) }),
            AppendText(t) => {
                let doc = self.document.root();
                let text = Text::new(t, *doc);
                NodeCast::from_temporary(text)
            }
        }
    }
}

impl<'a> TreeSink<TrustedNodeAddress> for servohtmlparser::Sink {
    fn get_document(&mut self) -> TrustedNodeAddress {
        let doc = self.document.root();
        let node: JSRef<Node> = NodeCast::from_ref(*doc);
        node.to_trusted_node_address()
    }

    fn same_node(&self, x: TrustedNodeAddress, y: TrustedNodeAddress) -> bool {
        x == y
    }

    fn elem_name(&self, target: TrustedNodeAddress) -> QualName {
        let node: Root<Node> = unsafe { JS::from_trusted_node_address(target).root() };
        let elem: JSRef<Element> = ElementCast::to_ref(*node)
            .expect("tried to get name of non-Element in HTML parsing");
        QualName {
            ns: elem.namespace().clone(),
            local: elem.local_name().clone(),
        }
    }

    fn create_element(&mut self, name: QualName, attrs: Vec<Attribute>)
            -> TrustedNodeAddress {
        let doc = self.document.root();
        let elem = Element::create(name, None, *doc, ParserCreated).root();

        for attr in attrs.into_iter() {
            elem.set_attribute_from_parser(attr.name, attr.value, None);
        }

        let node: JSRef<Node> = NodeCast::from_ref(*elem);
        node.to_trusted_node_address()
    }

    fn create_comment(&mut self, text: String) -> TrustedNodeAddress {
        let doc = self.document.root();
        let comment = Comment::new(text, *doc);
        let node: Root<Node> = NodeCast::from_temporary(comment).root();
        node.to_trusted_node_address()
    }

    fn append_before_sibling(&mut self,
            sibling: TrustedNodeAddress,
            new_node: NodeOrText<TrustedNodeAddress>) -> Result<(), NodeOrText<TrustedNodeAddress>> {
        // If there is no parent, return the node to the parser.
        let sibling: Root<Node> = unsafe { JS::from_trusted_node_address(sibling).root() };
        let parent = match sibling.parent_node() {
            Some(p) => p.root(),
            None => return Err(new_node),
        };

        let child = self.get_or_create(new_node).root();
        assert!(parent.InsertBefore(*child, Some(*sibling)).is_ok());
        Ok(())
    }

    fn parse_error(&mut self, msg: MaybeOwned<'static>) {
        debug!("Parse error: {:s}", msg);
    }

    fn set_quirks_mode(&mut self, mode: QuirksMode) {
        let doc = self.document.root();
        doc.set_quirks_mode(mode);
    }

    fn append(&mut self, parent: TrustedNodeAddress, child: NodeOrText<TrustedNodeAddress>) {
        let parent: Root<Node> = unsafe { JS::from_trusted_node_address(parent).root() };
        let child = self.get_or_create(child).root();

        // FIXME(#3701): Use a simpler algorithm and merge adjacent text nodes
        assert!(parent.AppendChild(*child).is_ok());
    }

    fn append_doctype_to_document(&mut self, name: String, public_id: String, system_id: String) {
        let doc = self.document.root();
        let doc_node: JSRef<Node> = NodeCast::from_ref(*doc);
        let doctype = DocumentType::new(name, Some(public_id), Some(system_id), *doc);
        let node: Root<Node> = NodeCast::from_temporary(doctype).root();

        assert!(doc_node.AppendChild(*node).is_ok());
    }

    fn add_attrs_if_missing(&mut self, target: TrustedNodeAddress, attrs: Vec<Attribute>) {
        let node: Root<Node> = unsafe { JS::from_trusted_node_address(target).root() };
        let elem: JSRef<Element> = ElementCast::to_ref(*node)
            .expect("tried to set attrs on non-Element in HTML parsing");
        for attr in attrs.into_iter() {
            elem.set_attribute_from_parser(attr.name, attr.value, None);
        }
    }

    fn remove_from_parent(&mut self, _target: TrustedNodeAddress) {
        error!("remove_from_parent not implemented!");
    }

    fn mark_script_already_started(&mut self, node: TrustedNodeAddress) {
        let node: Root<Node> = unsafe { JS::from_trusted_node_address(node).root() };
        let script: Option<JSRef<HTMLScriptElement>> = HTMLScriptElementCast::to_ref(*node);
        script.map(|script| script.mark_already_started());
    }

    fn complete_script(&mut self, node: TrustedNodeAddress) {
        let node: Root<Node> = unsafe { JS::from_trusted_node_address(node).root() };
        let script: Option<JSRef<HTMLScriptElement>> = HTMLScriptElementCast::to_ref(*node);
        script.map(|script| script.prepare());
    }
}

pub fn parse_html(document: JSRef<Document>,
                  input: HTMLInput,
                  base_url: Option<Url>,
                  load_response: Option<LoadResponse>) {
    let parser = ServoHTMLParser::new(base_url.clone(), document).root();
    let parser: JSRef<ServoHTMLParser> = *parser;

    task_state::enter(IN_HTML_PARSER);

    match input {
        InputString(s) => {
            parser.parse_chunk(s);
        }
        InputUrl(url) => {
            let load_response = load_response.unwrap();
            match load_response.metadata.content_type {
                Some((ref t, _)) if t.as_slice().eq_ignore_ascii_case("image") => {
                    let page = format!("<html><body><img src='{:s}' /></body></html>", base_url.as_ref().unwrap().serialize());
                    parser.parse_chunk(page);
                },
                _ => {
                    for msg in load_response.progress_port.iter() {
                        match msg {
                            Payload(data) => {
                                // FIXME: use Vec<u8> (html5ever #34)
                                let data = UTF_8.decode(data.as_slice(), DecodeReplace).unwrap();
                                parser.parse_chunk(data);
                            }
                            Done(Err(err)) => {
                                panic!("Failed to load page URL {:s}, error: {:s}", url.serialize(), err);
                            }
                            Done(Ok(())) => break,
                        }
                    }
                }
            }
        }
    }

    parser.finish();

    task_state::exit(IN_HTML_PARSER);

    debug!("finished parsing");
}
