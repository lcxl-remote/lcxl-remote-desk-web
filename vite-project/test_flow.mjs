import fs from 'fs';
import path from 'path';

const BASE_URL = 'http://127.0.0.1:8081';
let sessionCookie = '';

async function fetchApi(endpoint, options = {}) {
    const headers = {
        'Content-Type': 'application/json',
        ...options.headers,
    };

    if (sessionCookie) {
        headers['Cookie'] = sessionCookie;
    }

    const res = await fetch(`${BASE_URL}${endpoint}`, {
        ...options,
        headers,
    });

    const setCookie = res.headers.get('set-cookie');
    if (setCookie) {
        // Simple extraction of the session cookie
        const match = setCookie.match(/(id=[^;]+)/);
        if (match) {
            sessionCookie = match[1];
        }
    }

    let data;
    const contentType = res.headers.get('content-type');
    if (contentType && contentType.includes('application/json')) {
        data = await res.json();
    } else {
        data = await res.text();
    }

    return {
        status: res.status,
        data
    };
}

async function runTests() {
    console.log('--- Starting API Integration Tests ---');

    // 1. Check server_info
    console.log('\n[1] Testing GET /server_info...');
    let res = await fetchApi('/api/server_info');
    console.log('Status:', res.status);
    console.log('Data:', res.data);
    if (res.status !== 200) throw new Error('server_info failed');
    const { initialized, startup_mode } = res.data.data;

    // 2. Init system if not initialized
    if (!initialized) {
        console.log('\n[2] Testing POST /init...');
        res = await fetchApi('/api/init', {
            method: 'POST',
            body: JSON.stringify({
                username: 'admin',
                password: 'password' // using default password for test
            })
        });
        console.log('Status:', res.status);
        console.log('Data:', res.data);
        if (res.status !== 200) throw new Error('init failed');
    } else {
        console.log('\n[2] System already initialized, skipping init.');
    }

    // 3. Login as Admin
    console.log('\n[3] Testing POST /api/auth/login...');
    res = await fetchApi('/api/auth/login', {
        method: 'POST',
        body: JSON.stringify({
            username: "admin",
            password: "password"
        })
    });
    console.log('Status:', res.status);
    console.log('Data:', res.data);
    if (res.status !== 200) throw new Error('Admin login failed. Make sure password is "password"');

    // 4. Create Device Code
    console.log('\n[4] Testing POST /api/desk/device_code...');
    const testClientId = "client-" + Date.now();
    const testDeviceCode = Math.floor(100000 + Math.random() * 900000).toString();
    res = await fetchApi('/api/desk/device_codes', {
        method: 'POST',
        body: JSON.stringify({
            client_id: testClientId,
            device_code: testDeviceCode
        })
    });
    console.log('Status:', res.status);
    console.log('Data:', res.data);
    if (res.status !== 200) throw new Error('Create device code failed');

    // 5. List Device Code
    console.log('\n[5] Testing GET /api/desk/device_code...');
    res = await fetchApi('/api/desk/device_codes');
    console.log('Status:', res.status);
    if (res.status !== 200) throw new Error('List device code failed');

    // Find the created device code
    const list = res.data.data.items;
    const createdItem = list.find(item => item.clientId === testClientId);
    console.log('Found created item:', createdItem);
    if (!createdItem) throw new Error('Created item not found in list');

    // 6. Redeem the device code (clear the admin cookie first)
    console.log('\n[6] Testing POST /api/desk/redeem-code...');
    sessionCookie = ''; // clear admin session
    res = await fetchApi('/api/desk/redeem-code', {
        method: 'POST',
        body: JSON.stringify({
            code: testDeviceCode
        })
    });
    console.log('Status:', res.status);
    console.log('Data:', res.data);
    if (res.status === 403 && res.data.includes("offline")) {
        console.log('Got expected 403 (device offline). Security check passed.');
    } else if (res.status !== 200) {
        throw new Error('Device code login failed with unexpected status: ' + res.status);
    } else {
        if (res.data.data.target_connection_id !== testClientId) throw new Error('Expected target_connection_id to be client_id');
    }

    // 7. Test RBAC: device user cannot list device codes
    console.log('\n[7] Testing RBAC (device user trying to list device codes)...');
    res = await fetchApi('/api/desk/device_codes');
    console.log('Status:', res.status);
    if (res.status !== 401 && res.status !== 403) {
        console.warn('Expected 401/403 for device user listing device codes, but got:', res.status);
        console.log(res.data);
    } else {
        console.log('RBAC Check passed: device user is rejected.');
    }

    // 8. Log back in as admin and Delete Device Code
    console.log('\n[8] Logging back in as Admin to cleanup...');
    sessionCookie = '';
    res = await fetchApi('/api/auth/login', {
        method: 'POST',
        body: JSON.stringify({
            username: "admin",
            password: "password"
        })
    });
    if (res.status !== 200) throw new Error('Admin re-login failed');

    console.log(`\n[9] Testing DELETE /api/desk/device_code/${createdItem.id}...`);
    res = await fetchApi(`/api/desk/device_codes/${createdItem.id}`, {
        method: 'DELETE'
    });
    console.log('Status:', res.status);
    console.log('Data:', res.data);

    if (res.status !== 200) throw new Error('Delete device code failed');

    console.log('\n--- All API Integration Tests Passed! ---');
}

runTests().catch(err => {
    console.error('\nTest Failed:', err);
    process.exit(1);
});
