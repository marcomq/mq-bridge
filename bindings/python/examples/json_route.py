from pathlib import Path
import threading
import time

from mq_bridge import Publisher, Route


CONFIG_PATH = Path(__file__).with_name("memory.yaml")


def handle_order(data):
    print("decoded data:", data)
    return {"accepted": True, "order_id": data["order_id"]}


route = Route.from_file(str(CONFIG_PATH), "orders_route").add_handler(
    "order.created",
    handle_order,
)
publisher = Publisher.from_file(str(CONFIG_PATH), "orders_publisher")


def drive() -> None:
    time.sleep(0.2)
    publisher.send_json(
        {"order_id": 42, "status": "created"},
        {"kind": "order.created"},
    )
    time.sleep(0.2)
    route.stop()


threading.Thread(target=drive, daemon=True).start()
route.run()
