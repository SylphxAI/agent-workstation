import sys
from pathlib import Path

import gi

gi.require_version("Gtk", "3.0")
from gi.repository import Gtk


dialog = Gtk.Dialog(title="AT-SPI Test Dialog")
entry = Gtk.Entry()
entry.get_accessible().set_name("Fixture Entry")
check = Gtk.CheckButton(label="Fixture Checked")
check.set_active(True)
apply = Gtk.Button(label="Fixture Apply")
box = dialog.get_content_area()
for widget in (entry, check, apply):
    box.add(widget)

receipt = Path(sys.argv[1])
apply.connect("clicked", lambda _: receipt.write_text(entry.get_text(), encoding="utf-8"))
dialog.show_all()
Gtk.main()
