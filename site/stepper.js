// Shared step player: Stepper({stages, onShow(i, prev)}). Stages carry title + cap.
// Owns the controls, dots, keyboard, autoplay and reduced-motion handling so each
// animation page only supplies what a step looks like.
(function () {
  var reduce = window.matchMedia && window.matchMedia('(prefers-reduced-motion: reduce)').matches;
  window.Stepper = function (o) {
    var S = o.stages, cur = -1, timer = null, $ = function (id) { return document.getElementById(id); };
    var nav = $('dots-nav'), playBtn = $('play');
    S.forEach(function (st, i) {
      var b = document.createElement('button'); b.type = 'button'; b.textContent = i;
      b.setAttribute('aria-label', 'Step ' + i + ': ' + st.title);
      b.addEventListener('click', function () { pause(); show(i); });
      nav.appendChild(b);
    });
    function show(i) {
      i = Math.max(0, Math.min(S.length - 1, i));
      var prev = cur; cur = i;
      $('stage-title').textContent = S[i].title;
      $('stage-count').textContent = 'Step ' + (i + 1) + ' of ' + S.length;
      $('caption').textContent = S[i].cap;
      Array.prototype.forEach.call(nav.children, function (b, k) {
        if (k === i) b.setAttribute('aria-current', 'step'); else b.removeAttribute('aria-current');
      });
      $('prev').disabled = i === 0;
      $('next').disabled = i === S.length - 1;
      o.onShow(i, prev, reduce);
    }
    function pause() { if (timer) { clearInterval(timer); timer = null; } playBtn.textContent = 'Play'; playBtn.setAttribute('aria-pressed', 'false'); }
    function play() {
      if (cur >= S.length - 1) show(0);
      playBtn.textContent = 'Pause'; playBtn.setAttribute('aria-pressed', 'true');
      timer = setInterval(function () { if (cur >= S.length - 1) pause(); else show(cur + 1); }, 5200);
    }
    playBtn.addEventListener('click', function () { timer ? pause() : play(); });
    $('prev').addEventListener('click', function () { pause(); show(cur - 1); });
    $('next').addEventListener('click', function () { pause(); show(cur + 1); });
    document.addEventListener('keydown', function (e) {
      if (/INPUT|TEXTAREA/.test(e.target.tagName)) return;
      if (e.key === 'ArrowRight') { pause(); show(cur + 1); }
      else if (e.key === 'ArrowLeft') { pause(); show(cur - 1); }
      else if (e.key === ' ' && e.target.tagName !== 'BUTTON') { e.preventDefault(); timer ? pause() : play(); }
    });
    show(0);
    if (!reduce) setTimeout(function () { if (cur === 0 && !timer) play(); }, 1200);
    return { show: show, current: function () { return cur; } };
  };
})();
