(() => {
  'use strict';

  const $ = (selector) => document.querySelector(selector);
  const $$ = (selector) => [...document.querySelectorAll(selector)];

  // Keep every diagram useful in the static document; JS only adds controls.
  const cycleText = [
    'Bedrock chooses intercepts, memory permissions, counter state and pending events. This is host work before the CPU enters guest mode.',
    'The CPU validates the VMCS or VMCB and switches to guest state. The guest is still executing on the physical core.',
    'Ordinary guest instructions execute directly. Caches, TLBs and branch prediction still matter to wall-clock performance.',
    'A configured intercept, nested-page permission failure, debug event or deadline mechanism returns control to the host.',
    'Bedrock accounts for retired instructions, emulates the event, revises its proofs and enters the guest again.'
  ];
  let cycleStep = 0;
  function renderCycle() {
    $$('.cycle-lane').forEach((node, index) => node.classList.toggle('active', index === cycleStep));
    $('#cycle-count').textContent = `${cycleStep + 1} / ${cycleText.length}`;
    $('#cycle-description').textContent = cycleText[cycleStep];
  }
  $('#cycle-prev').addEventListener('click', () => { cycleStep = (cycleStep + cycleText.length - 1) % cycleText.length; renderCycle(); });
  $('#cycle-next').addEventListener('click', () => { cycleStep = (cycleStep + 1) % cycleText.length; renderCycle(); });
  renderCycle();

  const accessExplanations = {
    read: ['A normal read', 'Guest page tables map the virtual address to a GPA. EPT/NPT maps that GPA to host memory. If both allow the read, the CPU continues.'],
    write: ['A guarded write', 'Guest Linux may permit the write, while Bedrock marks the code or page-table GPA read-only in EPT/NPT. The CPU exits before the write, allowing proof invalidation.'],
    fetch: ['A guarded fetch', 'Bedrock can mark a code GPA non-executable in NPT. Fetching it exits before the instruction runs, so the next page can be checked and added to a safe plan.']
  };
  function renderAccess(kind) {
    $$('#translation-figure [data-access]').forEach((button) => button.classList.toggle('active', button.dataset.access === kind));
    $('#access-label').textContent = accessExplanations[kind][0];
    $('#permission-detail').textContent = accessExplanations[kind][1];
  }
  $$('#translation-figure [data-access]').forEach((button) => button.addEventListener('click', () => renderAccess(button.dataset.access)));
  renderAccess('read');

  function place(element, start, end) {
    element.style.left = `${Math.max(0, start) / 1200 * 100}%`;
    element.style.width = `${Math.max(0, end - Math.max(0, start)) / 1200 * 100}%`;
  }
  function renderDeadline() {
    const target = Number($('#deadline-target').value);
    $('#deadline-value').textContent = target.toLocaleString();
    place($('#intel-arm'), target - 100, target - 12);
    place($('#intel-final'), target - 12, target);
    place($('#amd-arm'), target - 290, target - 80);
    place($('#amd-final'), target - 80, target);
    [$('#intel-marker'), $('#amd-marker')].forEach((marker) => { marker.style.left = `${target / 1200 * 100}%`; });
    $('#deadline-description').textContent = `At logical instruction ${target.toLocaleString()}, both paths must expose the event at the same guest-visible boundary. Shaded widths are explanatory only; they are not measured skid, margin or CPU cycle counts.`;
  }
  $('#deadline-target').addEventListener('input', renderDeadline);
  renderDeadline();

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

  const themeToggle = $('#theme-toggle');
  let savedTheme = null;
  try { savedTheme = localStorage.getItem('bedrock-guide-theme'); } catch (_) { /* local file storage may be unavailable */ }
  if (savedTheme === 'dark') document.documentElement.dataset.theme = 'dark';
  if (savedTheme === 'dark') themeToggle.setAttribute('aria-label', 'Switch to light theme');
  themeToggle.addEventListener('click', () => {
    const theme = document.documentElement.dataset.theme === 'dark' ? 'light' : 'dark';
    document.documentElement.dataset.theme = theme;
    themeToggle.setAttribute('aria-label', `Switch to ${theme === 'dark' ? 'light' : 'dark'} theme`);
    try { localStorage.setItem('bedrock-guide-theme', theme); } catch (_) { /* optional preference */ }
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
