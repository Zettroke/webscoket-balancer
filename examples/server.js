const WebSocket = require('ws');

const wss1 = new WebSocket.Server({ port: 1338, clientTracking: false });

var cnt = 0

wss1.on('connection', function connection(ws) {
    console.log('1338 - connected', cnt++);
    ws.on('message', function incoming(message) {
        let s = '1338: ' + message;
        console.log(s);
        ws.send(s);
    });

    ws.send('something');
});

const wss2 = new WebSocket.Server({ port: 1339, clientTracking: false });

wss2.on('connection', function connection(ws) {
	
    console.log('1339 - connected', cnt++);
    ws.on('message', function incoming(message) {
        let s = '1339: ' + message;
        console.log(s);
        ws.send(s);
    });

    ws.send('something');
});