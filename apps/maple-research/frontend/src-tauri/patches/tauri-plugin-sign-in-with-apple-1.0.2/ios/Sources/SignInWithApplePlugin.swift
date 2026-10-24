//
//  SignInWithApplePlugin.swift
//  tauri-plugin-sign-in-with-apple
//
//  Created by Manaf Mhamdi Alaoui on 10.11.2024.
//

import SwiftRs
import Tauri
import UIKit
import WebKit

class AppleIDAuthorizationRequest: Decodable {
    let scope: [String]
    let nonce: String?
    let state: String?
}

class AppleIDAuthorizationResponse: Encodable {
    let userIdentifier: String?

    let givenName: String?
    let familyName: String?
    let email: String?

    let authorizationCode: String
    let identityToken: String?
    let state: String?

    init(userIdentifier: String?, givenName: String?, familyName: String?, email: String?, authorizationCode: String, identityToken: String?, state: String?) {
        self.userIdentifier = userIdentifier
        self.givenName = givenName
        self.familyName = familyName
        self.email = email
        self.authorizationCode = authorizationCode
        self.identityToken = identityToken
        self.state = state
    }
}

class SignInWithApplePlugin: Plugin {
    var lastController: SignInWithAppleAuthorizationController?
    
    @objc public func get_apple_id_credential(_ invoke: Invoke) throws {
        let args = try invoke.parseArgs(AppleIDAuthorizationRequest.self)

        // store to keep alive
        let controller = SignInWithAppleAuthorizationController(invoke)
        lastController = controller
        
        controller.authorize(req: args)
    }
}

@_cdecl("init_plugin_sign_in_with_apple")
func initPlugin() -> Plugin {
  return SignInWithApplePlugin()
}
