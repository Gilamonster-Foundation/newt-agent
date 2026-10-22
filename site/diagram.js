// Node/edge scene for Stepper. spec = {nodes:[{id,label,sub,x,y,kind}], stages:[{title,cap,show:[ids],hot:[ids],
// state:{id:'ok'|'deny'}, tag:{id:text}, edges:[[from,to,label,dash?]], note}]}. x,y are percentages.
(function () {
  var NS = 'http://www.w3.org/2000/svg';
  window.Diagram = function (scene, spec) {
    var els = {};
    var svg = document.createElementNS(NS, 'svg');
    svg.innerHTML = '<defs><marker id="arr" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse"><path d="M0 0L10 5L0 10z" fill="currentColor"/></marker></defs>';
    svg.style.color = 'var(--ink)';
    scene.appendChild(svg);
    spec.nodes.forEach(function (n) {
      var d = document.createElement('div');
      d.className = 'nd hide' + (n.kind ? ' kind-' + n.kind : '');
      d.style.left = n.x + '%'; d.style.top = n.y + '%';
      var b = document.createElement('b'); b.textContent = n.label; d.appendChild(b);
      if (n.sub) { var s = document.createElement('small'); s.textContent = n.sub; d.appendChild(s); }
      scene.appendChild(d); els[n.id] = d;
    });
    var last = 0;
    function center(id) {
      var r = els[id].getBoundingClientRect(), p = scene.getBoundingClientRect();
      return { x: r.left - p.left + r.width / 2, y: r.top - p.top + r.height / 2, w: r.width / 2, h: r.height / 2 };
    }
    function edgePt(a, b) { // point on a's box boundary toward b
      var dx = b.x - a.x, dy = b.y - a.y, k = Math.max(Math.abs(dx) / a.w, Math.abs(dy) / a.h) || 1;
      return { x: a.x + dx / k, y: a.y + dy / k };
    }
    function draw(i) {
      Array.prototype.slice.call(svg.querySelectorAll('.edge,.elabel')).forEach(function (e) { e.remove(); });
      var all = spec.stages[i].edges || [];
      all.forEach(function (e) {
        if (els[e[0]].classList.contains('hide') || els[e[1]].classList.contains('hide')) return;
        var a = center(e[0]), b = center(e[1]), p = edgePt(a, b), q = edgePt(b, a);
        var twin = all.some(function (o) { return o[0] === e[1] && o[1] === e[0]; });
        if (twin) { // opposite-direction pair: shift each to its own side
          var len = Math.hypot(q.x - p.x, q.y - p.y) || 1, s = (e[0] < e[1] ? -1 : 1) * 9;
          var ox = -(q.y - p.y) / len * s, oy = (q.x - p.x) / len * s;
          p = { x: p.x + ox, y: p.y + oy }; q = { x: q.x + ox, y: q.y + oy };
        }
        var l = document.createElementNS(NS, 'line');
        l.setAttribute('class', 'edge' + (e[3] ? ' dash' : ''));
        l.setAttribute('x1', p.x); l.setAttribute('y1', p.y); l.setAttribute('x2', q.x); l.setAttribute('y2', q.y);
        l.setAttribute('marker-end', 'url(#arr)'); svg.appendChild(l);
        if (e[2]) {
          var t = document.createElementNS(NS, 'text'); t.setAttribute('class', 'elabel');
          t.setAttribute('x', (p.x + q.x) / 2); t.setAttribute('y', (p.y + q.y) / 2 - 5); t.textContent = e[2]; svg.appendChild(t);
        }
      });
    }
    window.addEventListener('resize', function () { draw(last); });
    return function onShow(i) {
      last = i; var st = spec.stages[i];
      spec.nodes.forEach(function (n) {
        var el = els[n.id];
        el.classList.toggle('hide', (st.show || []).indexOf(n.id) < 0);
        el.classList.toggle('hot', (st.hot || []).indexOf(n.id) >= 0);
        var s = (st.state || {})[n.id];
        el.classList.toggle('ok', s === 'ok'); el.classList.toggle('deny', s === 'deny');
        var old = el.querySelector('.tag'); if (old) old.remove();
        if ((st.tag || {})[n.id]) { var t = document.createElement('span'); t.className = 'tag'; t.textContent = st.tag[n.id]; el.appendChild(t); }
      });
      draw(i);
      var z = document.getElementById('zones'); if (z) z.textContent = st.note || '';
    };
  };
})();
