// 晾衣绳：和 App 里一样的时序——指针停在菜单栏 0.25 秒放下，离开 0.5 秒收回。
(function () {
  var reduce = window.matchMedia("(prefers-reduced-motion: reduce)").matches;

  function wireLine(hoverTarget, lineEl, downClass) {
    if (!hoverTarget || !lineEl) return;
    var enterTimer = null;
    var leaveTimer = null;

    function down() {
      clearTimeout(leaveTimer);
      lineEl.classList.add(downClass || "is-down");
    }
    function up() {
      clearTimeout(enterTimer);
      leaveTimer = setTimeout(function () {
        lineEl.classList.remove(downClass || "is-down");
      }, 500);
    }

    hoverTarget.addEventListener("pointerenter", function () {
      clearTimeout(leaveTimer);
      enterTimer = setTimeout(down, 250);
    });
    hoverTarget.addEventListener("pointerleave", up);
    // 键盘也能看：Tab 到菜单栏就放下。
    hoverTarget.addEventListener("focus", down);
    hoverTarget.addEventListener("blur", up);

    return { down: down, up: up };
  }

  var hero = wireLine(document.getElementById("menubar"), document.getElementById("hero-line"));

  // 刚打开页面时露一下脸，跟「录完自动露脸 2.5 秒」对应。
  if (hero && !reduce) {
    setTimeout(function () {
      hero.down();
      setTimeout(hero.up, 2600);
    }, 700);
  }

  // 晾衣绳那一节滚进视口时也自动放一次，让说明和画面对上。
  var miniBar = document.querySelector(".mini-bar");
  var miniLine = document.querySelector(".mini-line");
  var mini = wireLine(miniBar, miniLine);
  if (mini && miniLine && "IntersectionObserver" in window) {
    var io = new IntersectionObserver(
      function (entries) {
        entries.forEach(function (entry) {
          if (entry.isIntersecting) {
            setTimeout(mini.down, 300);
            io.disconnect();
          }
        });
      },
      { threshold: 0.4 }
    );
    io.observe(miniLine);
  }
})();
