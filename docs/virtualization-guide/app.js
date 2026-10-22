(() => {
  'use strict';

  const $ = (selector) => document.querySelector(selector);
  const $$ = (selector) => [...document.querySelectorAll(selector)];

  // Keep every diagram useful in the static document; JS only adds controls.
  const exits = [
    { key: 'npt', label: 'NPT faults', count: 21837491 },
    { key: 'clock', label: 'RDTSC / RDTSCP', count: 13984167 },
    { key: 'mtf', label: 'Synthetic MTF', count: 5512847 },
    { key: 'other', label: 'Other exits', count: 1478834 }
  ];
  const total = exits.reduce((sum, row) => sum + row.count, 0);
  function renderExits() {
    const halfNpt = $('#toggle-half-npt').checked;
    const halfClock = $('#toggle-half-clock').checked;
    const chart = $('#exit-bars');
    chart.replaceChildren();
    for (const item of exits) {
      const row = document.createElement('div');
      row.className = `exit-row ${item.key}`;
      const label = document.createElement('span');
      label.className = 'label'; label.textContent = item.label;
      const meter = document.createElement('div');
      meter.className = 'meter';
      const fill = document.createElement('span');
      fill.className = 'fill'; fill.style.width = `${item.count / total * 100}%`;
      const selected = (item.key === 'npt' && halfNpt) || (item.key === 'clock' && halfClock);
      if (selected) {
        const ghost = document.createElement('span');
        ghost.className = 'ghost'; ghost.style.width = '50%'; fill.appendChild(ghost);
      }
      meter.appendChild(fill);
      const value = document.createElement('span');
      value.className = 'value'; value.textContent = `${(item.count / 1e6).toFixed(2)}M`;
      row.append(label, meter, value);
      chart.appendChild(row);
    }
    const highlighted = (halfNpt ? exits[0].count / 2 : 0) + (halfClock ? exits[1].count / 2 : 0);
    $('#exit-description').textContent = highlighted
      ? `Hatched portions represent ${Math.round(highlighted).toLocaleString()} hypothetical exits removed. ${Math.round(total - highlighted).toLocaleString()} exits would remain before any new work. This is arithmetic, not a wall-time prediction.`
      : 'These controls mark a hypothetical number of exits removed. They do not predict seconds saved.';
    chart.setAttribute('aria-label', exits.map((item) => `${item.label}: ${item.count.toLocaleString()}`).join('; '));
  }
  $('#toggle-half-npt').addEventListener('change', renderExits);
  $('#toggle-half-clock').addEventListener('change', renderExits);
  renderExits();

  const toyStories = {
    a: ['0x12 · 0x34', '0x12 → frame 0xA0 · read-only', 'physical 0xA034', 'the offset 0x34 is unchanged'],
    b: ['0x1A · 0x34', '0x1A → frame 0xB0 · read/write', 'physical 0xB034', 'a different virtual page reaches a different frame'],
    missing: ['0x20 · 0x34', 'no entry for virtual page 0x20', 'page fault (#PF)', 'the guest kernel decides how to handle the missing page'],
    write: ['0x12 · 0x34', '0x12 → frame 0xA0 · read-only', 'page fault (#PF)', 'the mapping exists, but this entry denies writing']
  };
  $$('#toy-paging-lab [data-toy]').forEach((button) => button.addEventListener('click', () => {
    $$('#toy-paging-lab [data-toy]').forEach((item) => item.classList.toggle('active', item === button));
    const [virtual, entry, outcome, status] = toyStories[button.dataset.toy];
    $('#toy-virtual').textContent = virtual; $('#toy-entry').textContent = entry;
    $('#toy-outcome').textContent = outcome; $('#toy-status').textContent = status;
    $('#toy-paging-lab').classList.toggle('toy-fault', ['missing', 'write'].includes(button.dataset.toy));
  }));


  const intelModes = {
    app: 'A guest application runs at CPL3 in VMX non-root mode. It is an ordinary process from the guest kernel’s point of view, and the host’s VMX controls still apply.',
    kernel: 'Guest Linux runs at CPL0 in VMX non-root mode. It can manage guest processes and page tables, while EPT and the host’s intercept controls retain authority.',
    host: 'Bedrock’s kernel-side VMM runs at CPL0 in VMX root mode. It configures the VMCS, handles exits and owns the physical virtualization boundary.'
  };
  $$('#intel-mode-lab [data-intel-mode]').forEach((button) => button.addEventListener('click', () => {
    $$('#intel-mode-lab [data-intel-mode]').forEach((item) => item.classList.toggle('active', item === button));
    $$('#intel-mode-lab .mode-cell').forEach((item) => item.classList.remove('selected'));
    $(`#mode-${button.dataset.intelMode}`).classList.add('selected');
    $('#intel-mode-explanation').textContent = intelModes[button.dataset.intelMode];
  }));
  function renderBudget() {
    const millions = Number($('#budget-exits').value);
    const micros = Number($('#budget-cost').value);
    $('#budget-exits-value').textContent = `${millions}M`;
    $('#budget-cost-value').textContent = `${micros.toFixed(1)} µs`;
    $('#budget-time').textContent = `${Math.round(millions * micros)} seconds`;
    $('#budget-equation').textContent = `${millions}M × ${micros.toFixed(1)} µs`;
  }
  $('#budget-exits').addEventListener('input', renderBudget);
  $('#budget-cost').addEventListener('input', renderBudget);
  renderBudget();

  const coreCases = {
    warm: ['100', '100', 'mostly guest instruction work', 'same visible instruction sequence', 'short execution path'],
    miss: ['100', '320', 'waiting for data from a slower cache level or RAM', 'same visible instruction sequence', 'illustrative memory stall'],
    branch: ['100', '165', 'discarded speculative work after a wrong prediction', 'same retired instructions', 'illustrative branch penalty'],
    exit: ['100', '850', 'mode transition and host handler work', 'same guest-visible sequence', 'illustrative VM-exit overhead']
  };
  $$('#core-lab [data-core]').forEach((button) => button.addEventListener('click', () => {
    $$('#core-lab [data-core]').forEach((item) => item.classList.toggle('active', item === button));
    const [instructions, cycles, reason, instructionNote, cycleNote] = coreCases[button.dataset.core];
    $('#core-instructions').textContent = instructions;
    $('#core-cycles').textContent = cycles;
    $('#core-reason').textContent = reason;
    $('#core-instruction-note').textContent = instructionNote;
    $('#core-cycle-note').textContent = cycleNote;
  }));

  let retirements = 0;
  function renderPmu() {
    $('#pmu-count').textContent = `${retirements} / 8`;
    $('#pmu-overflow').textContent = retirements >= 8 ? 'pending' : 'clear';
    $('#pmu-rip').textContent = retirements >= 8 ? 'not from overflow alone' : 'no notification yet';
    $('#pmu-description').textContent = retirements >= 8
      ? 'The threshold is crossed. A counter event alone does not report an exact stopping RIP; delivery can lag.'
      : 'The guest retires instructions while the counter advances. No host callback is needed for each retirement.';
  }
  $('#pmu-step').addEventListener('click', () => { retirements = Math.min(8, retirements + 1); renderPmu(); });
  $('#pmu-run').addEventListener('click', () => { retirements = 8; renderPmu(); });
  $('#pmu-reset').addEventListener('click', () => { retirements = 0; renderPmu(); });
  renderPmu();

  function renderSkid() {
    const delay = Number($('#skid-slider').value);
    $('#skid-value').textContent = String(delay);
    const track = $('#counter-track'); track.replaceChildren();
    for (let i = 94; i <= 112; i++) {
      const cell = document.createElement('div');
      cell.className = 'counter-cell' + (i === 100 ? ' target' : '') + (i === 100 + delay ? ' observed' : '');
      const number = document.createElement('b'); number.textContent = String(i);
      const label = document.createElement('small');
      label.textContent = i === 100 && delay === 0 ? 'target + exit' : i === 100 ? 'target' : i === 100 + delay ? 'observed exit' : '';
      cell.append(number, label); track.appendChild(cell);
    }
    $('#counter-explain').textContent = delay === 0
      ? 'Target: 100. The hypothetical exit lands at 100; no instruction-delay correction is needed.'
      : `Target: 100. An ordinary notification observed after ${100 + delay} retirements would be ${delay} instructions late.`;
  }
  $('#skid-slider').addEventListener('input', renderSkid); renderSkid();

  $$('#replay-simulation [data-replay]').forEach((button) => button.addEventListener('click', () => {
    const before = button.dataset.replay === 'before';
    $$('#replay-simulation [data-replay]').forEach((item) => item.classList.toggle('active', item === button));
    const x = before ? 516 : 782;
    for (const id of ['replay-timer-marker']) { $(`#${id}`).setAttribute('x1', x); $(`#${id}`).setAttribute('x2', x); }
    $('#replay-timer-circle').setAttribute('cx', x);
    $('#replay-timer-label').setAttribute('x', before ? '464' : '728');
    $('#replay-timer-label').textContent = before ? 'TIMER BEFORE' : 'TIMER AFTER';
    $('#replay-observation').textContent = before ? 'Thread B wakes while A still holds the lock.' : 'Thread B wakes after A releases the lock.';
    $('#replay-explanation').textContent = before
      ? 'If the timer wakes thread B before A releases the lock, B can observe the lock as held. Delivering it after the release can expose a different state.'
      : 'If the timer wakes thread B after A releases the lock, B can observe it as free. The same code ran, but the input arrived at a different logical boundary.';
  }));

  const hazards = {
    five: ['5 exact breakpoints required · cannot fit', 'With four DR0–DR3 slots, the planner must narrow the region or fall back.', []],
    mask: ['4 slots after a masked merge', 'One masked breakpoint covers two hazards, but may also trap at innocent offsets in the aligned range.', [0, 1]],
    proof: ['4 slots after a reachability proof', 'A hazard needs no breakpoint only if all guarded paths prove that the running region cannot reach it.', [4]]
  };
  $$('#hazard-lab [data-hazard]').forEach((button) => button.addEventListener('click', () => {
    $$('#hazard-lab [data-hazard]').forEach((item) => item.classList.toggle('active', item === button));
    const [slots, explanation, selected] = hazards[button.dataset.hazard];
    $('#hazard-slots').textContent = slots; $('#hazard-explain').textContent = explanation;
    $$('#hazard-lab .hazard-strip > div').forEach((item, index) => item.classList.toggle('selected', selected.includes(index)));
  }));

  const themeToggle = $('#theme-toggle');
  let savedTheme = null;
  try { savedTheme = localStorage.getItem('bedrock-guide-theme'); } catch (_) { /* storage is optional */ }
  if (savedTheme === 'dark') document.documentElement.dataset.theme = 'dark';
  themeToggle.addEventListener('click', () => {
    const next = document.documentElement.dataset.theme === 'dark' ? 'light' : 'dark';
    document.documentElement.dataset.theme = next;
    themeToggle.setAttribute('aria-label', `Switch to ${next === 'dark' ? 'light' : 'dark'} theme`);
    try { localStorage.setItem('bedrock-guide-theme', next); } catch (_) { /* storage is optional */ }
  });

  $('#term-search').addEventListener('input', () => {
    const query = $('#term-search').value.trim().toLowerCase();
    let visible = 0;
    $$('.term-atlas [data-term]').forEach((term) => {
      const match = term.textContent.toLowerCase().includes(query);
      term.hidden = !match;
      if (match) visible++;
    });
    $('#term-count').textContent = query ? `${visible} matching term${visible === 1 ? '' : 's'}.` : 'All terms shown. Type to filter by name or explanation.';
  });

  const sectionLinks = $$('#toc a');
  const sections = sectionLinks.map((link) => $(link.getAttribute('href'))).filter(Boolean);
  const mobileToc = $('#mobile-toc');
  mobileToc.addEventListener('change', () => $(mobileToc.value)?.scrollIntoView({ behavior: 'smooth' }));
  let scheduled = false;
  function updateProgress() {
    const max = document.documentElement.scrollHeight - innerHeight;
    $('#read-progress').style.width = `${max > 0 ? Math.min(100, scrollY / max * 100) : 0}%`;
    const threshold = innerHeight * 0.34;
    let current = sections[0];
    for (const section of sections) {
      if (section.getBoundingClientRect().top <= threshold) current = section;
      else break;
    }
    sectionLinks.forEach((link) => link.classList.toggle('active', link.hash === `#${current.id}`));
    mobileToc.value = `#${current.id}`;
    scheduled = false;
  }
  window.addEventListener('scroll', () => { if (!scheduled) { scheduled = true; requestAnimationFrame(updateProgress); } }, { passive: true });
  updateProgress();
})();
