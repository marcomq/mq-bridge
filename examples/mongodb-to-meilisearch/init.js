try {
  rs.status();
} catch (e) {
  rs.initiate({ _id: "rs0", members: [{ _id: 0, host: "mongo:27017" }] });
}
while (!db.hello().isWritablePrimary) {
  sleep(500);
}

const shop = db.getSiblingDB("shop");
if (shop.products.countDocuments() === 0) {
  shop.products.insertMany([
    { name: "Wireless headphones", description: "Over-ear, noise cancelling, 30 hours of battery.", price: 79.0, tags: ["audio"] },
    { name: "Mechanical keyboard", description: "Tenkeyless keyboard with brown switches.", price: 99.0, tags: ["desk"] },
    { name: "USB-C charger", description: "65 W charger for laptops and phones.", price: 35.0, tags: ["power"] },
  ]);
}
