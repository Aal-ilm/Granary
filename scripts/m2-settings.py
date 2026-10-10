#!/usr/bin/env python3
"""Add or remove Granary's <mirror> and <server> in Maven's settings.xml.

Used by granary-client.sh. Works on the XML tree (not on raw lines), keeps
the rest of the file as it was, and only ever touches what sits between its
own marker comments.

  m2-settings.py use FILE URL   add the mirror (and the server, when the
                                GRANARY_TOKEN environment variable is set)
  m2-settings.py public FILE    remove the mirror, keep the server
  m2-settings.py remove FILE    remove both
  m2-settings.py status FILE    print where Maven Central requests go

The token is read from the environment so it never shows up in `ps`.
"""

import os
import sys
import xml.dom.minidom as minidom
from xml.dom import Node

MIRROR = (">>> granary mirror (managed by granary-client) >>>", "<<< granary mirror <<<")
SERVER = (">>> granary server (managed by granary-client) >>>", "<<< granary server <<<")
CENTRAL = "https://repo.maven.apache.org/maven2/ (default)"

NEW_FILE = """<?xml version="1.0" encoding="UTF-8"?>
<settings xmlns="http://maven.apache.org/SETTINGS/1.2.0"
          xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"
          xsi:schemaLocation="http://maven.apache.org/SETTINGS/1.2.0 https://maven.apache.org/xsd/settings-1.2.0.xsd">
</settings>
"""


def die(msg):
    print(f"error: {msg}", file=sys.stderr)
    sys.exit(1)


def load(path, create):
    if not os.path.exists(path):
        if not create:
            return None
        return minidom.parseString(NEW_FILE)
    try:
        return minidom.parse(path)
    except Exception as e:  # noqa: BLE001 - any parse error means "don't touch it"
        die(f"{path} is not valid XML ({e}); fix it by hand, then run this again")


def save(doc, path):
    # minidom joins the declaration, top-level comments and the root onto
    # one line; keep each on its own line as the file had them.
    parts = ['<?xml version="1.0" encoding="UTF-8"?>']
    parts += [n.toxml() for n in doc.childNodes]
    data = ("\n".join(parts) + "\n").encode("utf-8")
    tmp = path + ".granary-tmp"
    old_umask = os.umask(0o077)
    try:
        with open(tmp, "wb") as f:
            f.write(data)
    finally:
        os.umask(old_umask)
    os.replace(tmp, path)


def elements(parent, name):
    return [
        n
        for n in parent.childNodes
        if n.nodeType == Node.ELEMENT_NODE and n.localName == name
    ]


def text(el, name):
    found = elements(el, name)
    if not found:
        return ""
    return "".join(
        n.data for n in found[0].childNodes if n.nodeType == Node.TEXT_NODE
    ).strip()


def indent_of(el):
    """The whitespace before `el` on its line."""
    prev = el.previousSibling
    if prev is not None and prev.nodeType == Node.TEXT_NODE and "\n" in prev.data:
        tail = prev.data.rsplit("\n", 1)[1]
        if not tail.strip():
            return tail
    return ""


def is_marker(node, text_):
    return node.nodeType == Node.COMMENT_NODE and node.data.strip() == text_


def in_block(node, markers):
    """Whether `node` sits between a pair of markers."""
    inside = False
    for n in node.parentNode.childNodes:
        if is_marker(n, markers[0]):
            inside = True
        elif is_marker(n, markers[1]):
            inside = False
        elif n is node:
            return inside
    return False


def strip_block(parent, markers):
    """Remove the marked block (markers included) from `parent`."""
    removing = False
    for n in list(parent.childNodes):
        if is_marker(n, markers[0]):
            removing = True
            # Take the line break and indent in front of the block with it.
            prev = n.previousSibling
            if prev is not None and prev.nodeType == Node.TEXT_NODE and not prev.data.strip():
                parent.removeChild(prev)
        if removing:
            parent.removeChild(n)
        if is_marker(n, markers[1]):
            removing = False


def drop_if_empty(el):
    """Remove a container we may have created once nothing is left in it."""
    if any(
        n.nodeType != Node.TEXT_NODE or n.data.strip() for n in el.childNodes
    ):
        return
    prev = el.previousSibling
    if prev is not None and prev.nodeType == Node.TEXT_NODE and not prev.data.strip():
        el.parentNode.removeChild(prev)
    el.parentNode.removeChild(el)


def container(doc, root, name):
    """The <mirrors> or <servers> element, created at the end if missing."""
    found = elements(root, name)
    if found:
        return found[0]
    el = doc.createElementNS(root.namespaceURI, name)
    unit = indent_of_children(root) or "  "
    last = root.lastChild
    if last is not None and last.nodeType == Node.TEXT_NODE and not last.data.strip():
        root.insertBefore(doc.createTextNode("\n" + unit), last)
        root.insertBefore(el, last)
    else:
        root.appendChild(doc.createTextNode("\n" + unit))
        root.appendChild(el)
        root.appendChild(doc.createTextNode("\n"))
    el.appendChild(doc.createTextNode("\n" + unit))
    return el


def indent_of_children(el):
    for n in el.childNodes:
        if n.nodeType == Node.ELEMENT_NODE:
            ind = indent_of(n)
            if ind:
                return ind[len(indent_of(el)) :] or ind
    return ""


def append_block(doc, parent, markers, name, fields):
    """Append `<!-- begin --><name>fields</name><!-- end -->` to `parent`."""
    base = indent_of(parent)
    unit = indent_of_children(parent.parentNode) or "  "
    inner = base + unit
    # Insert before the whitespace that precedes </parent>.
    anchor = parent.lastChild
    if not (anchor is not None and anchor.nodeType == Node.TEXT_NODE and not anchor.data.strip()):
        anchor = doc.createTextNode("\n" + base)
        parent.appendChild(anchor)
    else:
        anchor.data = "\n" + base

    def put(node):
        parent.insertBefore(doc.createTextNode("\n" + inner), anchor)
        parent.insertBefore(node, anchor)

    put(doc.createComment(f" {markers[0]} "))
    el = doc.createElementNS(parent.namespaceURI, name)
    for key, value in fields:
        el.appendChild(doc.createTextNode("\n" + inner + unit))
        child = doc.createElementNS(parent.namespaceURI, key)
        child.appendChild(doc.createTextNode(value))
        el.appendChild(child)
    el.appendChild(doc.createTextNode("\n" + inner))
    put(el)
    put(doc.createComment(f" {markers[1]} "))


def check_conflicts(root, path):
    """Refuse to add ours next to the user's own mirror or server for it."""
    for mirrors in elements(root, "mirrors"):
        for m in elements(mirrors, "mirror"):
            if in_block(m, MIRROR):
                continue
            of = [s.strip() for s in text(m, "mirrorOf").split(",")]
            if text(m, "id") == "granary" or any(s in ("*", "external:*", "central") for s in of):
                die(
                    f"{path} already has its own mirror for Maven Central "
                    f"(id {text(m, 'id') or '?'}). Remove it first, then run this again"
                )
    for servers in elements(root, "servers"):
        for s in elements(servers, "server"):
            if not in_block(s, SERVER) and text(s, "id") == "granary":
                die(f"{path} already has its own <server> with id granary. Remove it first, then run this again")


def cmd_use(path, url):
    doc = load(path, create=True)
    root = doc.documentElement
    if root.localName != "settings":
        die(f"{path} does not start with <settings>")
    check_conflicts(root, path)
    token = os.environ.get("GRANARY_TOKEN", "")

    for name, markers in (("servers", SERVER), ("mirrors", MIRROR)):
        for el in elements(root, name):
            strip_block(el, markers)
    if token:
        servers = container(doc, root, "servers")
        append_block(
            doc, servers, SERVER, "server",
            [("id", "granary"), ("username", "token"), ("password", token)],
        )
    mirrors = container(doc, root, "mirrors")
    append_block(
        doc, mirrors, MIRROR, "mirror",
        [("id", "granary"), ("mirrorOf", "central"), ("url", url)],
    )
    save(doc, path)


def cmd_strip(path, blocks):
    doc = load(path, create=False)
    if doc is None:
        return
    root = doc.documentElement
    for name, markers in blocks:
        for el in elements(root, name):
            strip_block(el, markers)
            drop_if_empty(el)
    save(doc, path)


def cmd_status(path):
    doc = load(path, create=False)
    if doc is None:
        print(CENTRAL)
        return
    for mirrors in elements(doc.documentElement, "mirrors"):
        for m in elements(mirrors, "mirror"):
            of = [s.strip() for s in text(m, "mirrorOf").split(",")]
            if any(s in ("*", "external:*", "central") for s in of):
                print(text(m, "url") or "(mirror without a url)")
                return
    print(CENTRAL)


def main(argv):
    if len(argv) == 4 and argv[1] == "use":
        cmd_use(argv[2], argv[3])
    elif len(argv) == 3 and argv[1] == "public":
        cmd_strip(argv[2], [("mirrors", MIRROR)])
    elif len(argv) == 3 and argv[1] == "remove":
        cmd_strip(argv[2], [("mirrors", MIRROR), ("servers", SERVER)])
    elif len(argv) == 3 and argv[1] == "status":
        cmd_status(argv[2])
    else:
        die("usage: m2-settings.py use FILE URL | public FILE | remove FILE | status FILE")


if __name__ == "__main__":
    main(sys.argv)
