var ws = require("nodejs-websocket")

// Scream server example: "hi" -> "HI!!!"
var server1 = ws.createServer(function (conn) {
    console.log("1338: New connection")
    conn.on("text", function (str) {
        console.log("1338: Received "+str)
        conn.sendText(str.toUpperCase()+"!!!")
    })
    conn.on("close", function (code, reason) {
        console.log("Connection closed")
    })
}).listen(1338);
var server2 = ws.createServer(function (conn) {
    console.log("1339: New connection")
    conn.on("text", function (str) {
        console.log("1339: Received "+str)
        conn.sendText(str.toUpperCase()+"!!!")
    })
    conn.on("close", function (code, reason) {
        console.log("Connection closed")
    })
}).listen(1339);