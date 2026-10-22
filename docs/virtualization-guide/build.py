#!/usr/bin/env python3
"""Assemble the source chapters into the standalone, dependency-free guide."""

from pathlib import Path
import html
import re


ROOT = Path(__file__).resolve().parent
source = (ROOT / "index.template.html").read_text()
source = source.replace("<!-- FOUNDATIONS -->", (ROOT / "foundations.html").read_text())
source = source.replace("<!-- EVENTS VISUAL -->", (ROOT / "events-visual.html").read_text())
source = source.replace("<!-- WALK VISUAL -->", (ROOT / "walk-visual.html").read_text())
source = source.replace("<!-- PROCESS VISUAL -->", (ROOT / "process-visual.html").read_text())
source = source.replace("<!-- INSTRUCTION VISUAL -->", (ROOT / "instruction-visual.html").read_text())
source = source.replace("<!-- DEADLINE VISUAL -->", (ROOT / "deadline-visual.html").read_text())
source = source.replace("<!-- MEMORY VISUAL -->", (ROOT / "memory-visual.html").read_text())
source = source.replace("<!-- BRIDGES -->", (ROOT / "bridges.html").read_text())
source = source.replace("<!-- PMU VISUAL -->", (ROOT / "pmu-visual.html").read_text())
source = source.replace("<!-- INTEL DEEP -->", (ROOT / "intel-deep.html").read_text())
source = source.replace("<!-- AMD DEEP -->", (ROOT / "amd-deep.html").read_text())
source = source.replace("<!-- INTEL STOP VISUAL -->", (ROOT / "intel-stop-visual.html").read_text())
source = source.replace("<!-- INTEL MODE VISUAL -->", (ROOT / "intel-mode-visual.html").read_text())
source = source.replace("<!-- AMD STOP VISUAL -->", (ROOT / "amd-stop-visual.html").read_text())
source = source.replace("<!-- ROGPT VISUAL -->", (ROOT / "rogpt-visual.html").read_text())
source = source.replace('<p>Intel\'s VMCS selects exits', (ROOT / "intel-worked.html").read_text() + '\n<p>Intel\'s VMCS selects exits', 1)
source = source.replace('<p>AMD SVM uses <code>VMRUN</code>', (ROOT / "amd-worked.html").read_text() + '\n<p>AMD SVM uses <code>VMRUN</code>', 1)
source = source.replace("<!-- HAZARD VISUAL -->", (ROOT / "hazard-visual.html").read_text())
source = source.replace('<p>The useful first-order model is', (ROOT / "performance-deep.html").read_text() + '\n<p>The useful first-order model is', 1)
source = source.replace('<section id="registers"', (ROOT / "microarchitecture.html").read_text() + '\n<section id="registers"', 1)
source = source.replace("<!-- CORE TIMELINE -->", (ROOT / "core-timeline.html").read_text())
source = source.replace('<section id="memory"', (ROOT / "world-switch.html").read_text() + '\n<section id="memory"', 1)
source = source.replace('<section id="determinism"', (ROOT / "replay-primer.html").read_text() + '\n<section id="determinism"', 1)
source = source.replace("<!-- REPLAY VISUAL -->", (ROOT / "replay-visual.html").read_text())
source = source.replace('<section id="intel"', (ROOT / "dst-system.html").read_text() + '\n<section id="intel"', 1)
source = source.replace('<section id="deadline"', (ROOT / "architecture-compare.html").read_text() + '\n<section id="deadline"', 1)
source = source.replace('<section id="atlas"', (ROOT / "glossary-deep.html").read_text() + '\n<section id="atlas"', 1)

for section_id, fragment in (
    ("determinism", "determinism-deep.html"),
    ("deadline", "deadline-deep.html"),
    ("bedrock", "bedrock-deep.html"),
    ("gap", "gap-deep.html"),
):
    pattern = rf'(<section id="{section_id}".*?)(</section>)'
    source, count = re.subn(pattern, lambda m: m.group(1) + '\n' + (ROOT / fragment).read_text() + '\n' + m.group(2), source, count=1, flags=re.S)
    if count != 1:
        raise SystemExit(f"Could not add {fragment} to {section_id}")

sections = re.findall(r'<section\b[^>]*\bid="([^"]+)"[^>]*>', source)
labels = {
    "start": "An instruction", "registers": "CPU state", "processes": "Processes",
    "microarchitecture": "Inside the core", "world-switch": "World switches",
    "replay-primer": "Replay boundaries",
    "glossary-deep": "Term atlas",
    "architecture-compare": "Intel and AMD compared",
    "dst-system": "A DST system",
    "paging": "Page walks", "events": "Faults and interrupts",
    "virtualization": "Virtualization", "memory": "Two translations",
    "tlb": "Cached translations", "counting": "Counters and deadlines",
    "determinism": "The DST contract", "intel": "Intel architecture",
    "amd": "AMD architecture", "deadline": "The precise stop",
    "bedrock": "The codebase", "cost": "Where time goes",
    "measurements": "Measurements", "gap": "The gap", "outlook": "Can it close?",
    "atlas": "Code and sources",
}
missing = set(sections) - labels.keys()
if missing:
    raise SystemExit(f"Add navigation labels for: {sorted(missing)}")
if len(sections) != len(set(sections)):
    raise SystemExit("Duplicate chapter ids")

for i, section in enumerate(sections, 1):
    pattern = rf'(<section\b[^>]*\bid="{re.escape(section)}"[^>]*><div class="section-index">).*?(</div>)'
    source, replaced = re.subn(pattern, lambda match: f'{match.group(1)}{i:02d} / {html.escape(labels[section]).upper()}{match.group(2)}', source, count=1, flags=re.S)
    if replaced != 1:
        raise SystemExit(f"Missing section index for {section}")

options = "".join(
    f'<option value="#{section}">{i:02d} · {html.escape(labels[section])}</option>'
    for i, section in enumerate(sections, 1)
)
links = "".join(
    f'<a href="#{section}"><b>{i:02d}</b> {html.escape(labels[section])}</a>'
    for i, section in enumerate(sections, 1)
)
source = re.sub(
    r'(<select id="mobile-toc"[^>]*>).*?(</select>)',
    lambda match: match.group(1) + options + match.group(2),
    source,
    count=1,
    flags=re.S,
)
source = re.sub(
    r'(<nav id="toc">).*?(</nav>)',
    lambda match: match.group(1) + links + match.group(2),
    source,
    count=1,
    flags=re.S,
)
source = re.sub(r'(class="eyebrow">[^<]*<span>)01 / \d+(</span>)',
                rf'\g<1>01 / {len(sections):02d}\2', source, count=1)
source = re.sub(r'(<div class="hero-meta"><div><strong>)\d+(</strong><span>technical chapters)',
                rf'\g<1>{len(sections)}\2', source, count=1)
figures = source.count('class="figure interactive')
source = re.sub(r'(<span>technical chapters</span></div><div><strong>)\d+(</strong><span>interactive models)',
                rf'\g<1>{figures}\2', source, count=1)
(ROOT / "index.html").write_text(source)
print(f"Built {len(sections)} chapters, {figures} interactive figures")
