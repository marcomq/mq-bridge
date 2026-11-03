from pathlib import Path
import threading
import time

from mq_bridge import Publisher, Route


CONFIG_PATH = Path(__file__).with_name("memory.yaml")


def handle_raw(message):
    print("raw payload:", message.payload)
    print("raw metadata:", message.metadata)
    return b"ok"


route = Route.from_file(str(CONFIG_PATH), "orders_route").with_handler(handle_raw)
publisher = Publisher.from_file(str(CONFIG_PATH), "orders_publisher")

def drive() -> None:
    time.sleep(0.2)
    publisher.send(b'{"hello":"world"}', {"kind": "raw.demo"})
    time.sleep(0.2)
    route.stop()


threading.Thread(target=drive, daemon=True).start()
route.run()
